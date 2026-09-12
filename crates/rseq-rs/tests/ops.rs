use rseq_rs::Rseq;

#[test]
fn compare_exchange_and_add() {
    let Some(rseq) = Rseq::try_new() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    let thread = rseq.bind().expect("bind");
    let cpu = thread.cpu_id().expect("cpu");
    let words = rseq.words().expect("words");
    let w = words.get(cpu).expect("word");
    assert_eq!(thread.compare_exchange(w, 0, 7), Ok(0));
    assert_eq!(thread.compare_exchange(w, 0, 1), Err(7));
    assert_eq!(thread.fetch_add(w, 1), 7);
    assert_eq!(thread.compare_exchange(w, 8, 8), Ok(8));
}

#[test]
fn isolated_cpus() {
    let Some(rseq) = Rseq::try_new() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    if rseq.cpus() < 2 {
        eprintln!("skip: need two CPUs");
        return;
    }
    let words = rseq.words().expect("words");
    pin(0);
    let t0 = rseq.bind().expect("bind 0");
    let c0 = t0.cpu_id().expect("cpu 0");
    let w0 = words.get(c0).expect("word 0");
    assert_eq!(t0.compare_exchange(w0, 0, 11), Ok(0));
    pin(1);
    let t1 = rseq.bind().expect("bind 1");
    let c1 = t1.cpu_id().expect("cpu 1");
    let w1 = words.get(c1).expect("word 1");
    assert_eq!(t1.compare_exchange(w1, 0, 22), Ok(0));
    pin(0);
    let t0 = rseq.bind().expect("rebind 0");
    let w0 = words.get(t0.cpu_id().expect("cpu 0")).expect("word 0");
    assert_eq!(t0.compare_exchange(w0, 11, 11), Ok(11));
    pin(1);
    let t1 = rseq.bind().expect("rebind 1");
    let w1 = words.get(t1.cpu_id().expect("cpu 1")).expect("word 1");
    assert_eq!(t1.compare_exchange(w1, 22, 22), Ok(22));
}

fn pin(cpu: usize) {
    unsafe {
        let mut set = std::mem::zeroed::<libc::cpu_set_t>();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &raw const set);
    }
}
