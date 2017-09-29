extern crate rings;

mod from_deque {
    //tests from kinghajj/deque
    use std::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT};
    use std::sync::atomic::Ordering::SeqCst;
    use rings::mpmc::*;
    use rings::EnqueueErr::*;
    use rings::DequeueErr::*;

    use std::thread;

    #[test]
    fn smoke() {
        let (s, r) = channel(1).unwrap();
        assert_eq!(r.try_recv(), Err(NotEnoughInQueue));
        assert_eq!(r.clone().try_recv(), Err(NotEnoughInQueue));
        assert_eq!(s.try_send(1), Ok(()));
        assert_eq!(r.try_recv(), Ok(1));
        assert_eq!(s.try_send(3), Ok(()));
        assert_eq!(s.try_send(1), Err(NotEnoughRoom));
        assert_eq!(r.try_recv(), Ok(3));
        assert_eq!(s.try_send(4), Ok(()));
        assert_eq!(r.clone().try_recv(), Ok(4));
        assert_eq!(s.clone().try_send(2), Ok(()));
        assert_eq!(r.try_recv(), Ok(2));
        assert_eq!(s.clone().try_send(22), Ok(()));
        assert_eq!(r.clone().try_recv(), Ok(22));
        assert_eq!(r.try_recv(), Err(NotEnoughInQueue));
        assert_eq!(r.clone().try_recv(), Err(NotEnoughInQueue));
    }

    #[test]
    fn stealpush() {
        static AMT: isize = 100000;
        let (s, r) = channel(((AMT as u32) / 2).next_power_of_two()).unwrap();
        let t = thread::spawn(move || {
            let mut left = AMT;
            while left > 0 {
                match r.try_recv() {
                    Ok(i) => {
                        assert_eq!(i, AMT - left);
                        left -= 1;
                    }
                    Err(..) => { thread::yield_now() }
                }
            }
        });

        for i in 0..AMT {
            while let Err(..) = s.try_send(i) {
                thread::yield_now()
            }
        }

        t.join().unwrap();
    }

    #[test]
    fn stealpush_large() {
       static AMT: isize = 100000;
       let (s, r) = channel((AMT as u32) / 2).unwrap();
       let t = thread::spawn(move || {
           let mut left = AMT;
           while left > 0 {
               match r.try_recv() {
                   Ok((1, 10)) => { left -= 1; }
                   Ok(..) => panic!(),
                   Err(..) => { thread::yield_now() }
               }
           }
       });

        for _ in 0..AMT {
            while let Err(..) = s.try_send((1isize, 10isize)) {
                thread::yield_now()
            }
        }

        t.join().unwrap();
    }

    fn stampede(
        s: Sender<Box<isize>>, r: Receiver<Box<isize>>, nthreads: isize, amt: usize
    ) {
        for _ in 0..amt {
            while let Err(..) = s.try_send(Box::new(20)) {
                thread::yield_now()
            }
        }
        static HANDLED: AtomicUsize = ATOMIC_USIZE_INIT;

        let threads = (0..nthreads).map(|_| {
            let r = r.clone();
            thread::spawn(move || {
                while HANDLED.load(SeqCst) < amt {
                    match r.try_recv() {
                        Ok(ref i) if **i == 20 => {
                            HANDLED.fetch_add(1, SeqCst);
                        }
                        Ok(i) => panic!("invalid val {:?}", i),
                        Err(..) => { thread::yield_now() }
                    }
                }
            })
        }).collect::<Vec<_>>();

        for thread in threads.into_iter() {
            thread.join().unwrap();
        }
    }

    #[test]
    fn run_stampede() {
        let (s, r) = channel(10000).unwrap();
        stampede(s, r, 8, 10000);
    }

    #[test]
    fn many_stampede() {
        static AMT: usize = 4;
        let threads = (0..AMT).map(|_| {
            let (s, r) = channel(10000).unwrap();
            thread::spawn(move || {
                stampede(s, r, 4, 10000);
            })
        }).collect::<Vec<_>>();

        for thread in threads.into_iter() {
            thread.join().unwrap();
        }
    }
}// end from_deque
