use rseq_rs::{CpuId, Error, Rseq};

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
    assert_eq!(thread.compare_exchange(w, 0, 1), Err(Error::Miss(7)));
    assert_eq!(thread.fetch_add(w, 1), Ok(7));
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
    let thread = rseq.bind().expect("bind");
    let a = thread.cpu_id().expect("cpu");
    let b = (0..rseq.cpus())
        .filter_map(CpuId::new)
        .find(|id| *id != a)
        .expect("other cpu");
    if !pin(a.get()) {
        eprintln!("skip: pin {a:?}");
        return;
    }
    let t = rseq.bind().expect("bind a");
    let wa = words.get(t.cpu_id().expect("cpu a")).expect("word a");
    assert_eq!(t.compare_exchange(wa, 0, 11), Ok(0));
    if !pin(b.get()) {
        eprintln!("skip: pin {b:?}");
        return;
    }
    let t = rseq.bind().expect("bind b");
    let wb = words.get(t.cpu_id().expect("cpu b")).expect("word b");
    assert_eq!(t.compare_exchange(wb, 0, 22), Ok(0));
    if !pin(a.get()) {
        eprintln!("skip: re-pin {a:?}");
        return;
    }
    let t = rseq.bind().expect("rebind a");
    let wa = words.get(t.cpu_id().expect("cpu a")).expect("word a");
    assert_eq!(t.compare_exchange(wa, 11, 11), Ok(11));
    if !pin(b.get()) {
        eprintln!("skip: re-pin {b:?}");
        return;
    }
    let t = rseq.bind().expect("rebind b");
    let wb = words.get(t.cpu_id().expect("cpu b")).expect("word b");
    assert_eq!(t.compare_exchange(wb, 22, 22), Ok(22));
}

#[test]
fn wrong_cpu_aborts() {
    let Some(rseq) = Rseq::try_new() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    if rseq.cpus() < 2 {
        eprintln!("skip: need two CPUs");
        return;
    }
    let words = rseq.words().expect("words");
    let thread = rseq.bind().expect("bind");
    let here = thread.cpu_id().expect("cpu");
    if !pin(here.get()) {
        eprintln!("skip: pin {here:?}");
        return;
    }
    let thread = rseq.bind().expect("bind pinned");
    let here = thread.cpu_id().expect("cpu");
    let other = (0..rseq.cpus())
        .filter_map(CpuId::new)
        .find(|id| *id != here)
        .expect("other cpu");
    let w = words.get(other).expect("other word");
    assert_eq!(thread.compare_exchange(w, 0, 1), Err(Error::Abort));
    assert_eq!(thread.fetch_add(w, 1), Err(Error::Abort));
}

fn pin(cpu: u32) -> bool {
    let cpu = cpu as usize;
    unsafe {
        let mut set = std::mem::zeroed::<libc::cpu_set_t>();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &raw const set) == 0
    }
}
