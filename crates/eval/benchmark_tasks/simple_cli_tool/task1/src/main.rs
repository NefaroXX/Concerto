use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut text: Option<String> = None;
    let mut uppercase = false;
    let mut skip = false;
    for i in 1..args.len() {
        if skip {
            skip = false;
            continue;
        }
        if args[i] == "--text" {
            if i + 1 < args.len() {
                text = Some(args[i + 1].to_string());
                skip = true;
            }
        } else if args[i] == "--uppercase" {
            uppercase = true;
        }
    }
    let result = text.unwrap_or_else(|| String::new());
    if uppercase {
        println!("{}", result.to_uppercase());
    } else {
        println!("{}", result);
    }
}
