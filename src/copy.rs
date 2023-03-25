use std::env::args;
use std::fs;
use std::process::exit;

fn main() {
    if args().len() < 2 {
        println!("Missing path argument where to copy the koordinator executable to");
        exit(1);
    }
    let path = args().nth(1).unwrap();
    let err = fs::copy("/bin/koordinator", std::path::Path::new(&path).join("koordinator"));
    if err.is_err() {
        println!("Failed to copy /bin/koordinator {}", err.unwrap_err());
        exit(1);
    }
}
