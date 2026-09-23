fn main() {
    if std::env::args().nth(1).as_deref() == Some("--probe-network") {
        match std::net::TcpStream::connect("127.0.0.1:9") {
            Ok(_) => {
                eprintln!("network unexpectedly allowed");
                std::process::exit(2);
            }
            Err(error) => {
                eprintln!("network denied: {error}");
                std::process::exit(23);
            }
        }
    }
    std::process::exit(qaqh_sandbox::helper_main());
}
