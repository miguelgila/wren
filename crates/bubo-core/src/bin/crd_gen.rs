use bubo_core::crd::{BuboJob, BuboQueue};
use kube::CustomResourceExt;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("bubojob") => {
            print!("{}", serde_yaml::to_string(&BuboJob::crd()).unwrap());
        }
        Some("buboqueue") => {
            print!("{}", serde_yaml::to_string(&BuboQueue::crd()).unwrap());
        }
        Some("all") | None => {
            println!("---");
            print!("{}", serde_yaml::to_string(&BuboJob::crd()).unwrap());
            println!("---");
            print!("{}", serde_yaml::to_string(&BuboQueue::crd()).unwrap());
        }
        Some(other) => {
            eprintln!("Unknown CRD: {other}. Use: bubojob, buboqueue, or all");
            std::process::exit(1);
        }
    }
}
