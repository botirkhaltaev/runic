use std::process::Command;

#[test]
fn unknown_pointer_free_aborts() {
    AbortCase::new("unknown-free").assert_aborts();
}

#[test]
fn small_interior_pointer_free_aborts() {
    AbortCase::new("small-interior-free").assert_aborts();
}

#[test]
fn large_interior_pointer_free_aborts() {
    AbortCase::new("large-interior-free").assert_aborts();
}

#[test]
fn small_interior_pointer_realloc_aborts() {
    AbortCase::new("small-interior-realloc").assert_aborts();
}

#[test]
fn large_interior_pointer_realloc_aborts() {
    AbortCase::new("large-interior-realloc").assert_aborts();
}

struct AbortCase {
    name: &'static str,
}

impl AbortCase {
    const fn new(name: &'static str) -> Self {
        Self { name }
    }

    fn assert_aborts(self) {
        let status = Command::new(env!("CARGO_BIN_EXE_abort_case"))
            .arg(self.name)
            .status()
            .unwrap();

        assert!(!status.success());
    }
}
