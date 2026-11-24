//! The memory server over HTTP on a library of its own, for trying a harness against it:
//! `cargo run -p mcp --example http_serve -- <folder> <token> [user@host]` prints the port
//! and serves until stopped, the token standing for the host (`local` if none is given).

fn main() {
    let mut args = std::env::args().skip(1);
    let folder = std::path::PathBuf::from(args.next().expect("a folder for the library"));
    let token = args.next().expect("a token");
    let machine = args.next().map_or_else(model::EnvId::local, |host| model::EnvId::ssh(&host));
    std::fs::create_dir_all(&folder).expect("the folder");
    let library = folder.join("library.db");
    library::Library::open(&library).expect("the library");
    let tokens = std::sync::Arc::new(std::sync::RwLock::new([(token, machine)].into_iter().collect()));
    let port = mcp::http::start(0, library, folder.join("cache"), tokens).expect("the server");
    println!("{port}");
    loop {
        std::thread::park();
    }
}
