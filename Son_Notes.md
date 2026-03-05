
Add `scx_son`


To install son scheduler:
```sh
ls -d scheds/rust/scx_son | xargs -I{} cargo install --path {}  
```

To run the orchestra server:
```sh
cargo build -p orchestra
cargo run -p orchestra
```


schedA queues: t1, t2
schedB queues: t3, t4

Orchestra as the sched ext daemon:
1. Provide API for schedA and schedB, which will run on top of it
2. Monitor and check when schedext already enabled
3. If enabled, detach current scheduler 

