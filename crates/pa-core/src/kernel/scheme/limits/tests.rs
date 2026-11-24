//! Runaway cells against a real session kernel: each fails as its own cell,
//! and a second session's kernel in the same process keeps answering.

use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::tools::kernel_cell::{KernelExecuteOptions, KernelProvisioner as _};

/// What `code` did as one cell: its value's text, or the start of its error.
async fn cell(provisioner: &SchemeKernelProvisioner, code: &str) -> Result<String, String> {
    let result = provisioner
        .ensure(None, None)
        .await
        .unwrap()
        .execute(
            code,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        )
        .await
        .unwrap();
    match result.error {
        None => Ok(result.result.unwrap_or_default()),
        Some(error) => Err(error
            .evalue
            .trim_start_matches("Error: Generic: ")
            .chars()
            .take(40)
            .collect()),
    }
}

/// Every shape that ran a 2 MiB thread off its stack (and so ended the whole
/// process) when it was measured: source nested 172 deep, a value nested
/// 1,400 deep on its way to JSON, 2,800 deep in the reader, 17,000 deep
/// under a hash, a chain of 22,000 closures dropped. Each now ends as an
/// error of its own cell, or simply works, and the other session answers
/// after every one of them.
#[tokio::test(flavor = "multi_thread")]
async fn a_runaway_cell_fails_alone_and_another_session_keeps_working() {
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "echo",
        host_handler(|payload: HostRequestPayload| async move { Ok(payload.data) }),
    );
    let runaway = SchemeKernelProvisioner::new(handlers, "/");
    let other = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/");
    cell(
        &runaway,
        "(define (nest n acc) (if (= n 0) acc (nest (- n 1) (list acc))))
         (define deep (nest 100000 1))
         (define (parens n) (string-append (make-string n #\\() (make-string n #\\))))",
    )
    .await
    .unwrap();
    let too_deep_code = Err("the code is nested deeper than 1000 leve".to_string());
    let too_deep_data = Err("the value is nested deeper than 128 leve".to_string());
    let too_many = Err("the count is over 16777216, the most ele".to_string());
    let shapes = [
        // Source: written out, quoted, and built at run time.
        (
            format!("{}1{}", "(+ 1 ".repeat(5_000), ")".repeat(5_000)),
            too_deep_code.clone(),
        ),
        (
            format!(
                "(define q '{}1{}) 1",
                "(".repeat(100_000),
                ")".repeat(100_000)
            ),
            too_deep_code.clone(),
        ),
        (format!("{}1", "'".repeat(100_000)), too_deep_code.clone()),
        (
            "(eval-string (parens 100000))".to_string(),
            too_deep_code.clone(),
        ),
        (
            "(read (open-input-string (parens 100000)))".to_string(),
            too_deep_code.clone(),
        ),
        ("(eval deep)".to_string(), too_deep_code.clone()),
        // Values on their way to the host.
        (
            "(value->jsexpr-string deep)".to_string(),
            too_deep_data.clone(),
        ),
        (
            "(host-request \"echo\" deep)".to_string(),
            too_deep_data.clone(),
        ),
        (
            "(%native \"fs-read\" deep)".to_string(),
            Err("fs-read: the options are not plain data:".to_string()),
        ),
        ("(%cell \"todo\" deep)".to_string(), too_deep_data.clone()),
        (
            "(string->jsexpr (string-append (make-string 100000 #\\[) (make-string 100000 #\\])))"
                .to_string(),
            Err("string->jsexpr failed: recursion limit e".to_string()),
        ),
        // Depth the kernel cannot see coming: the cell's stack holds it.
        (
            "(define (count n) (if (= n 0) 0 (+ 1 (count (- n 1))))) (count 1000000)".to_string(),
            Ok("1000000".to_string()),
        ),
        (
            "(hash-ref (hash deep 1) deep)".to_string(),
            Ok("1".to_string()),
        ),
        (
            "(equal? deep (nest 100000 1))".to_string(),
            Ok("#true".to_string()),
        ),
        (
            "(string-length (to-string deep))".to_string(),
            Ok("259".to_string()),
        ),
        (
            "(define (chain n acc) (if (= n 0) acc (chain (- n 1) (lambda () acc))))
             (define links (chain 1000000 1)) (set! links 1) links"
                .to_string(),
            Ok("1".to_string()),
        ),
        // Counts to allocate by.
        (
            "(make-string 100000000000 #\\a)".to_string(),
            too_many.clone(),
        ),
        ("(make-vector 100000000000 0)".to_string(), too_many.clone()),
        ("(make-bytes 100000000000 0)".to_string(), too_many.clone()),
        ("(range 0 100000000000)".to_string(), too_many.clone()),
        ("(expt 7 100000000000)".to_string(), too_many.clone()),
        (
            format!("(define big \"{}\") 1", "a".repeat(super::SOURCE_BYTES)),
            Err("the cell is 1048593 bytes of code and th".to_string()),
        ),
    ];
    let mut seen = Vec::new();
    let mut expected = Vec::new();
    for (code, outcome) in shapes {
        seen.push((
            cell(&runaway, &code).await,
            cell(&other, "(+ 1 2)").await,
            cell(&runaway, "(+ 2 2)").await,
        ));
        expected.push((outcome, Ok("3".to_string()), Ok("4".to_string())));
    }
    assert_eq!(seen, expected);
}

/// A count a builtin allocates by, or would take as an unsigned number, and
/// a number JSON has no form for: each is refused as its cell's error. When
/// measured, the negative counts and the numbers panicked inside Steel, and
/// the large ones allocated without a limit (`read-bytes` of 100 GB ended
/// the process). The counts here are just over the limit, so one that got
/// through would be seen as a value, not by the process dying.
#[tokio::test(flavor = "multi_thread")]
async fn a_count_or_number_steel_cannot_take_is_its_cells_error() {
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "echo",
        host_handler(|payload: HostRequestPayload| async move { Ok(payload.data) }),
    );
    let provisioner = SchemeKernelProvisioner::new(handlers, "/");
    let too_many = Err("the count is over 16777216, the most ele".to_string());
    let negative = Err("the count is negative: a builtin that bu".to_string());
    let not_finite = Err("the value holds a number that is not fin".to_string());
    let shapes = [
        ("(make-string -1 #\\a)", negative.clone()),
        ("(make-bytes -1 0)", negative.clone()),
        ("(make-bytevector -1 0)", negative.clone()),
        ("(make-immutable-vector -1 0)", negative.clone()),
        ("(range-vec -3 0)", negative.clone()),
        ("(make-string (- (expt 2 80)) #\\a)", negative.clone()),
        ("(make-string (expt 2 80) #\\a)", too_many.clone()),
        (
            "(read-bytes -1 (open-input-bytevector (bytes 1 2 3)))",
            negative.clone(),
        ),
        ("(vector-length (range-vec 0 20000000))", too_many.clone()),
        (
            "(vector-length (make-immutable-vector 20000000 0))",
            too_many.clone(),
        ),
        (
            "(bytes-length (read-bytes 20000000 (open-input-bytevector (bytes 1 2 3))))",
            too_many.clone(),
        ),
        (
            "(bytes-length (read-bytevector 20000000 (open-input-bytevector (bytes 1 2 3))))",
            too_many.clone(),
        ),
        ("(value->jsexpr-string (/ 0. 0.))", not_finite.clone()),
        (
            "(host-request \"echo\" (hash 'a (list 1 (/ 1. 0.))))",
            not_finite.clone(),
        ),
        ("(%cell \"todo\" (- (/ 1. 0.)))", not_finite.clone()),
        // What the same builtins are for still works: a fill value, an
        // exponent and a shift are not counts.
        (
            "(list (make-vector 2 -1) (make-vector 1 100000000) (expt 2 -1) (arithmetic-shift 8 -1)
                   (range-vec 1 4) (make-string 2 #\\a) (value->jsexpr-string 1.5)
                   (read-bytes 2 (open-input-bytevector (bytes 1 2 3))))",
            Ok("(#(-1 -1) #(100000000) 1/2 4 #(1 2 3) \"aa\" \"1.5\" #u8(#x01 #x02))".to_string()),
        ),
    ];
    let mut seen = Vec::new();
    let mut expected = Vec::new();
    for (code, outcome) in shapes {
        seen.push((code, cell(&provisioner, code).await));
        expected.push((code, outcome));
    }
    assert_eq!(seen, expected);
}
