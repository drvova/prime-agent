use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

// The OS trust store, loaded once per process: reqwest reloads it on every
// `ClientBuilder::build`, which pays 75-85 ms per client on macOS. Keep only
// the roots `RootCertStore::add` accepts (reqwest's skip-invalid rule), so
// `add_root_certificate` below cannot fail the build on a real store's
// ancient certificates.
static NATIVE_CERTS: LazyLock<Vec<CertificateDer<'static>>> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    let mut certs = Vec::new();
    for cert in rustls_native_certs::load_native_certs().certs {
        if roots.add(cert.clone()).is_ok() {
            certs.push(cert);
        }
    }
    certs
});

/// The CA bundle `NODE_EXTRA_CA_CERTS` names for the calling session, loaded
/// once per path: sessions hosted in one process may each name their own.
fn extra_certs(path: Option<&OsString>) -> Arc<Vec<CertificateDer<'static>>> {
    static LOADED: Mutex<BTreeMap<OsString, Arc<Vec<CertificateDer<'static>>>>> =
        Mutex::new(BTreeMap::new());
    let Some(path) = path else {
        return Arc::default();
    };
    let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(certs) = loaded.get(path) {
        return Arc::clone(certs);
    }
    let mut certs = Vec::new();
    let read = (|| -> anyhow::Result<()> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_file_iter(path)? {
            let cert = cert?;
            roots.add(cert.clone())?;
            certs.push(cert);
        }
        anyhow::ensure!(!certs.is_empty(), "empty CA bundle");
        Ok(())
    })();
    if let Err(error) = read {
        tracing::warn!(
            path = %std::path::Path::new(path).display(),
            error = %error,
            "NODE_EXTRA_CA_CERTS could not be fully loaded; keeping certificates loaded before the error"
        );
    }
    // Node keeps the valid prefix and stops at the first PEM or DER error.
    let certs = Arc::new(certs);
    loaded.insert(path.clone(), Arc::clone(&certs));
    certs
}

/// The variables reqwest reads a client's proxies from, in its order of
/// preference for each scheme.
const PROXY_VARIABLES: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// What of the calling session's environment shapes an HTTP client: its
/// proxy variables where they differ from the process's own (reqwest reads
/// the process environment by itself, so a session hosted beside others
/// needs its own stated), and its extra CA bundle. Clients are shared only
/// between sessions for which this is equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientEnvironment {
    /// The session's value of each of [`PROXY_VARIABLES`]; `None` when all
    /// equal the process's, and reqwest's own detection applies.
    proxies: Option<[Option<String>; 8]>,
    extra_ca: Option<OsString>,
}

impl ClientEnvironment {
    /// The calling session's.
    #[must_use]
    pub fn current() -> Self {
        let own = PROXY_VARIABLES.map(|name| pa_types::session_scope::var(name).ok());
        let process = PROXY_VARIABLES.map(|name| std::env::var(name).ok());
        Self {
            proxies: (own != process).then_some(own),
            extra_ca: pa_types::session_scope::var_os("NODE_EXTRA_CA_CERTS"),
        }
    }
}

/// Make ring the process's rustls crypto provider unless one is already
/// installed (the one installed first stays, as reqwest 0.12 deferred to
/// it): reqwest 0.13 without aws-lc-rs refuses to build a client otherwise.
pub fn install_crypto_provider() {
    // `Err` only says a provider is already installed, which is the goal.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A client builder with the process's trust store and the calling
/// session's proxies and extra CA bundle.
///
/// # Panics
///
/// Never: every certificate below is a DER blob the root-store filter or
/// the PEM parser already accepted.
pub fn http_client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    let environment = ClientEnvironment::current();
    // Exactly these roots, verified by webpki: the OS store, the webpki
    // (Mozilla) roots and the session's extra bundle, as reqwest 0.12's
    // native-roots + webpki-roots features gave. Without `tls_certs_only`
    // reqwest 0.13 hands the roots to the platform verifier instead.
    let mut builder = reqwest::Client::builder().tls_certs_only(
        NATIVE_CERTS
            .iter()
            .chain(webpki_root_certs::TLS_SERVER_ROOT_CERTS)
            .chain(extra_certs(environment.extra_ca.as_ref()).iter())
            .map(|cert| reqwest::Certificate::from_der(cert).expect("CA certificate")),
    );
    if let Some([http_upper, http, https_upper, https, all_upper, all, no_upper, no]) =
        environment.proxies
    {
        // As reqwest reads the process's: a scheme's own variable, else
        // ALL_PROXY, and NO_PROXY exempts hosts from each.
        let exempt = no_upper.or(no);
        let all = all_upper.or(all);
        builder = builder.no_proxy();
        for (scheme, proxy) in [
            ("http", http_upper.or(http).or_else(|| all.clone())),
            ("https", https_upper.or(https).or(all)),
        ] {
            let Some(url) = proxy.filter(|url| !url.trim().is_empty()) else {
                continue;
            };
            let made = if scheme == "http" {
                reqwest::Proxy::http(&url)
            } else {
                reqwest::Proxy::https(&url)
            };
            match made {
                Ok(proxy) => {
                    builder = builder.proxy(
                        proxy.no_proxy(exempt.as_deref().and_then(reqwest::NoProxy::from_string)),
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "this session's {scheme} proxy variable is not a proxy URL; it is ignored");
                }
            }
        }
    }
    builder
}

/// The websocket TLS configuration for the calling session's extra CA
/// bundle, built once per bundle.
pub(crate) fn websocket_connector() -> tokio_tungstenite::Connector {
    static CONFIGS: Mutex<BTreeMap<Option<OsString>, Arc<rustls::ClientConfig>>> =
        Mutex::new(BTreeMap::new());
    let extra_ca = pa_types::session_scope::var_os("NODE_EXTRA_CA_CERTS");
    let extra = extra_certs(extra_ca.as_ref());
    let mut configs = CONFIGS.lock().unwrap_or_else(PoisonError::into_inner);
    let config = configs.entry(extra_ca).or_insert_with(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(NATIVE_CERTS.iter().cloned());
        roots.add_parsable_certificates(extra.iter().cloned());
        roots.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    });
    tokio_tungstenite::Connector::Rustls(Arc::clone(config))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{BufRead as _, Write as _};

    /// Two sessions hosted in one process, each launched with its own
    /// provider key and proxy, and one of them with an AWS profile: each
    /// reads its own key, only the one with a profile is authenticated to
    /// Bedrock, and each one's requests leave through its own proxy (on the
    /// provider client they share), never the other's.
    #[test]
    fn each_hosted_session_uses_its_own_credentials_and_proxy() {
        let session = |name: &'static str, profile: Option<&str>| {
            let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = proxy.local_addr().unwrap();
            // The proxy answers one request and reports its request line.
            let proxied = std::thread::spawn(move || {
                let (stream, _) = proxy.accept().unwrap();
                let mut line = String::new();
                std::io::BufReader::new(&stream)
                    .read_line(&mut line)
                    .unwrap();
                (&stream)
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .unwrap();
                line.trim_end().to_string()
            });
            let scope = pa_types::session_scope::SessionScope::new(
                name.to_string(),
                std::env::temp_dir(),
                [
                    ("ANTHROPIC_API_KEY".to_string(), format!("key-of-{name}")),
                    ("HTTP_PROXY".to_string(), format!("http://{address}")),
                ]
                .into_iter()
                .chain(profile.map(|profile| ("AWS_PROFILE".to_string(), profile.to_string())))
                .collect::<BTreeMap<_, _>>(),
                // Whatever this process was started with stays out of it.
                [
                    "http_proxy",
                    "NO_PROXY",
                    "no_proxy",
                    "AWS_PROFILE",
                    "AWS_ACCESS_KEY_ID",
                    "AWS_BEARER_TOKEN_BEDROCK",
                    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                    "AWS_WEB_IDENTITY_TOKEN_FILE",
                ]
                .into_iter()
                .filter(|hidden| *hidden != "AWS_PROFILE" || profile.is_none())
                .map(str::to_string)
                .collect::<BTreeSet<_>>(),
            );
            std::thread::spawn(move || {
                let _scope = pa_types::session_scope::enter(Some(scope));
                let status = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        crate::utils_inner::http::client()
                            .get(format!("http://{name}.session.test/whoami"))
                            .send()
                            .await
                            .unwrap()
                            .status()
                            .as_u16()
                    });
                (
                    crate::env_api_keys::get_env_api_key("anthropic"),
                    crate::env_api_keys::get_env_api_key("amazon-bedrock"),
                    status,
                    proxied.join().unwrap(),
                )
            })
        };
        let (first, second) = (session("first", Some("work")), session("second", None));
        let expected = |name: &str, bedrock: Option<&str>| {
            (
                Some(format!("key-of-{name}")),
                bedrock.map(str::to_string),
                200,
                format!("GET http://{name}.session.test/whoami HTTP/1.1"),
            )
        };
        assert_eq!(
            (first.join().unwrap(), second.join().unwrap()),
            (
                expected("first", Some("<authenticated>")),
                expected("second", None)
            )
        );
    }
}
