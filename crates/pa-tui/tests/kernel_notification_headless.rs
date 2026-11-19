//! Headless e2e for a kernel `notify(...)` ping: the daemon's `kernel_notification` session event
//! renders as one notice row with its level's mark, mid-turn, without the turn waiting on it.
#![cfg(unix)]

mod support;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::json;

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, write_json, write_session_event, MockSupervisor, Request,
};

fn serve(supervisor: &MockSupervisor) {
    let Some(stream) = accept_within_idle_window(&supervisor.listener) else {
        return;
    };
    serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
        match kind {
            "create" => write_json(writer, &create_response(id)),
            "attach" => {
                write_json(writer, &attach_response(id, "notify"));
                let mut events = writer.try_clone().expect("clone event socket");
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    for (message, level) in [("tests pass", "success"), ("CI is red", "warning")] {
                        write_session_event(
                            &mut events,
                            &json!({
                                "type": "kernel_notification",
                                "notification": { "message": message, "level": level },
                            }),
                        );
                    }
                });
            }
            _ => return false,
        }
        true
    });
}

#[test]
fn a_kernel_notification_renders_one_notice_row_per_ping() {
    let plan = HeadlessPlan {
        steps: vec![HeadlessStep::WaitMs(1200)],
        width: 100,
        height: 30,
    };
    let outcome = run_headless(plan, options, |socket| {
        let supervisor = MockSupervisor::bind(socket);
        move || serve(&supervisor)
    });
    let last = outcome.frames.last().expect("a frame painted");
    let rows: Vec<&str> = last
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('\u{2713}') || line.starts_with('\u{26a0}'))
        .collect();
    assert_eq!(rows, ["\u{2713} tests pass", "\u{26a0} CI is red"]);
}
