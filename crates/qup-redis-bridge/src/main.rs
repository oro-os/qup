use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "qup-redis-bridge", about = "Redis-backed QUP bridge")]
struct Cli {
    #[arg(long, default_value = "redis://127.0.0.1/")]
    redis_url: String,
}

fn main() {
    let cli = Cli::parse();
    let redis_url = cli.redis_url;

    let _ = redis::Value::Nil;
    let _ = qup::hello();

    println!("qup-redis-bridge scaffold targeting {redis_url}");
}
