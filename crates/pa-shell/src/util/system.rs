//! The machine and the shell's environment: `sleep`, `env`, `printenv`,
//! `which`, `date`, `uname`, `whoami`.

use std::io::Write;
use std::time::{Duration, Instant};

use super::{Arg, Failure, Invocation, Options, Outcome, UTILITIES};

/// `sleep NUMBER[smhd]...`
pub(super) fn sleep(invocation: &mut Invocation<'_>) -> Outcome {
    let mut total = 0.0f64;
    for arg in invocation.args {
        let (number, unit) = match arg.char_indices().next_back() {
            Some((at, 's')) => (&arg[..at], 1.0),
            Some((at, 'm')) => (&arg[..at], 60.0),
            Some((at, 'h')) => (&arg[..at], 3600.0),
            Some((at, 'd')) => (&arg[..at], 86400.0),
            Some(_) | None => (arg.as_str(), 1.0),
        };
        let seconds = number
            .parse::<f64>()
            .ok()
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .ok_or_else(|| Failure::Unsupported(format!("the interval {arg}")))?;
        total += seconds * unit;
    }
    if invocation.args.is_empty() {
        writeln!(
            invocation.stderr,
            "sleep: missing operand\nTry 'sleep --help' for more information."
        )?;
        return Ok(1);
    }
    let end = Instant::now() + Duration::try_from_secs_f64(total).unwrap_or(Duration::MAX);
    // In short naps, so a stop ends it within one of them.
    loop {
        invocation.shell.alive()?;
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(0);
        }
        std::thread::sleep(left.min(Duration::from_millis(10)));
    }
}

/// `env` and `printenv [VARIABLE...]`: the shell's exported variables. `env`
/// with anything to set or run is the program's.
pub(super) fn env(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut wanted = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Operand(variable) if invocation.name == "printenv" => wanted.push(variable),
            Arg::Operand(_) | Arg::Short(_) | Arg::Long(_) => {
                return Err(Options::unsupported(&arg))
            }
        }
    }
    if wanted.is_empty() {
        let mut variables = (invocation.shell.environment)();
        variables.sort();
        for (name, value) in variables {
            writeln!(invocation.stdout, "{name}={value}")?;
        }
        return Ok(0);
    }
    let mut status = 0;
    for variable in wanted {
        match (invocation.shell.environment)()
            .into_iter()
            .find(|(name, _)| name == variable)
        {
            Some((_, value)) => writeln!(invocation.stdout, "{value}")?,
            None => status = 1,
        }
    }
    Ok(status)
}

/// `which [-a] NAME...`: the program a name runs, or that the shell runs it
/// itself.
pub(super) fn which(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut names = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Operand(name) => names.push(name),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    let mut status = 0;
    for name in names {
        if let Some(program) = (invocation.shell.program)(name) {
            writeln!(invocation.stdout, "{}", program.display())?;
        } else if UTILITIES.contains(&name) {
            writeln!(invocation.stdout, "{name}: Prime Agent built-in")?;
        } else {
            status = 1;
        }
    }
    Ok(status)
}

/// `date [-u] [+FORMAT]`. A shell whose `TZ` names a zone, and a format
/// that prints the zone's name, need the system's zone data: the program's.
pub(super) fn date(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut utc, mut format) = (false, None);
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('u') | Arg::Long("utc" | "universal") => utc = true,
            Arg::Operand(operand) if operand.starts_with('+') && format.is_none() => {
                format = Some(&operand[1..]);
            }
            Arg::Operand(_) | Arg::Short(_) | Arg::Long(_) => {
                return Err(Options::unsupported(&arg))
            }
        }
    }
    match (invocation.shell.env)("TZ").as_deref() {
        None | Some("") => {}
        Some("UTC" | "UTC0" | "GMT" | "Etc/UTC") => utc = true,
        Some(zone) => return Err(Failure::Unsupported(format!("TZ={zone}"))),
    }
    let format = format.unwrap_or("%a %b %e %H:%M:%S %Z %Y");
    let zone_named = format.contains("%Z");
    if zone_named && !utc {
        return Err(Failure::Unsupported("a local time zone's name".to_string()));
    }
    // GNU's %N (nanoseconds) and %s are chrono's %f and %s; %Z is spelled
    // out, since chrono prints an offset for it.
    let format = format.replace("%N", "%f").replace("%Z", "UTC");
    let now = chrono::Utc::now();
    let mut rendered = String::new();
    let written = if utc {
        std::fmt::write(&mut rendered, format_args!("{}", now.format(&format)))
    } else {
        std::fmt::write(
            &mut rendered,
            format_args!("{}", now.with_timezone(&chrono::Local).format(&format)),
        )
    };
    if written.is_err() {
        return Err(Failure::Unsupported(format!("the format +{format}")));
    }
    writeln!(invocation.stdout, "{rendered}")?;
    Ok(0)
}

/// `uname [-s] [-n] [-r] [-v] [-m]`
pub(super) fn uname(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut wanted = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short(field @ ('s' | 'n' | 'r' | 'v' | 'm')) => wanted.push(field),
            Arg::Long("kernel-name") => wanted.push('s'),
            Arg::Long("nodename") => wanted.push('n'),
            Arg::Long("kernel-release") => wanted.push('r'),
            Arg::Long("kernel-version") => wanted.push('v'),
            Arg::Long("machine") => wanted.push('m'),
            Arg::Operand(_) | Arg::Short(_) | Arg::Long(_) => {
                return Err(Options::unsupported(&arg))
            }
        }
    }
    if wanted.is_empty() {
        wanted.push('s');
    }
    #[cfg(unix)]
    let field = {
        let system = nix::sys::utsname::uname().map_err(std::io::Error::from)?;
        move |field: char| -> Result<String, Failure> {
            let text = match field {
                's' => system.sysname(),
                'n' => system.nodename(),
                'r' => system.release(),
                'v' => system.version(),
                _ => system.machine(),
            };
            Ok(text.to_string_lossy().into_owned())
        }
    };
    #[cfg(not(unix))]
    let field = |field: char| -> Result<String, Failure> {
        match field {
            's' => Ok("Windows_NT".to_string()),
            'm' => Ok(std::env::consts::ARCH.to_string()),
            other => Err(Failure::Unsupported(format!("option -{other}"))),
        }
    };
    // Always in this order, however they were asked for.
    let mut shown = Vec::new();
    for candidate in ['s', 'n', 'r', 'v', 'm'] {
        if wanted.contains(&candidate) {
            shown.push(field(candidate)?);
        }
    }
    writeln!(invocation.stdout, "{}", shown.join(" "))?;
    Ok(0)
}

/// `whoami`
pub(super) fn whoami(invocation: &mut Invocation<'_>) -> Outcome {
    if let Some(arg) = invocation.args.first() {
        return Err(Failure::Unsupported(format!("the operand {arg}")));
    }
    #[cfg(unix)]
    let name = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .ok()
        .flatten()
        .map(|user| user.name);
    #[cfg(not(unix))]
    let name = (invocation.shell.env)("USERNAME");
    if let Some(name) = name {
        writeln!(invocation.stdout, "{name}")?;
        Ok(0)
    } else {
        writeln!(
            invocation.stderr,
            "whoami: cannot find name for the current user"
        )?;
        Ok(1)
    }
}
