use std::process::Command;

#[test]
fn unknown_pointer_free_aborts() {
    AbortCase::new(env!("CARGO_BIN_EXE_unknown_free")).assert_aborts();
}

#[test]
fn small_interior_pointer_free_aborts() {
    AbortCase::new(env!("CARGO_BIN_EXE_small_interior_free")).assert_aborts();
}

#[test]
fn large_interior_pointer_free_aborts() {
    AbortCase::new(env!("CARGO_BIN_EXE_large_interior_free")).assert_aborts();
}

struct AbortCase {
    binary: &'static str,
}

impl AbortCase {
    const fn new(binary: &'static str) -> Self {
        Self { binary }
    }

    fn assert_aborts(self) {
        let status = Command::new(self.binary).status().unwrap();

        assert!(!status.success());
    }
}
