//! Its own test binary, because `stop_bounded` is process-global and would stop
//! every bounded command the unit tests beside it run.

use std::time::{Duration, Instant};

/// A shutdown does not wait out a command's deadline, which is what kept a daemon
/// alive for the length of a review poll after its host had gone.
#[test]
fn stop_bounded_ends_a_running_command_well_before_its_deadline() {
    let dir = std::env::temp_dir();
    let argv: Vec<String> = ["sh", "-c", "sleep 30"].map(String::from).into();
    let began = Instant::now();
    let running =
        std::thread::spawn(move || orchd_base::proc::run_bounded(&dir, 60, &argv, "test"));
    std::thread::sleep(Duration::from_millis(200));
    orchd_base::proc::stop_bounded();

    let err = running
        .join()
        .unwrap()
        .expect_err("a stopped command is an error");
    assert!(
        err.to_string().contains("shutting down"),
        "unexpected error: {err:#}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "took {:?}",
        began.elapsed()
    );
}
