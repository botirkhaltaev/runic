use rseq_rs::Rseq;

#[test]
fn bind_when_available() {
    let Some(rseq) = Rseq::try_new() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    let thread = rseq.bind().expect("glibc area should bind after try_new");
    let cpu = thread.cpu_id().expect("cpu_id in range after bind");
    assert!(cpu.get() < rseq.cpus());
    assert!(rseq.fence(cpu));
}
