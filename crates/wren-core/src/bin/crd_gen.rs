use kube::CustomResourceExt;
use wren_core::crd::{WrenJob, WrenQueue, WrenUser};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("wrenjob") => {
            print!("{}", serde_yaml::to_string(&WrenJob::crd()).unwrap());
        }
        Some("wrenqueue") => {
            print!("{}", serde_yaml::to_string(&WrenQueue::crd()).unwrap());
        }
        Some("wrenuser") => {
            print!("{}", serde_yaml::to_string(&WrenUser::crd()).unwrap());
        }
        Some("all") | None => {
            println!("---");
            print!("{}", serde_yaml::to_string(&WrenJob::crd()).unwrap());
            println!("---");
            print!("{}", serde_yaml::to_string(&WrenQueue::crd()).unwrap());
            println!("---");
            print!("{}", serde_yaml::to_string(&WrenUser::crd()).unwrap());
        }
        Some(other) => {
            eprintln!("Unknown CRD: {other}. Use: wrenjob, wrenqueue, wrenuser, or all");
            std::process::exit(1);
        }
    }
}
