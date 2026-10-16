//! Resolves the application names on stdin, one per line, to icon data URLs,
//! printed as one JSON object. Lets scripts/screenshots.mjs show the icons the
//! app would, from the same lookup.

use std::io::BufRead;

fn main() {
    let mut icons = serde_json::Map::new();
    for name in std::io::stdin().lock().lines().map_while(Result::ok) {
        if let Some(url) = netmeter_lib::system::proc_icon::get_process_icon(&name) {
            icons.insert(name, url.into());
        }
    }
    println!("{}", serde_json::Value::Object(icons));
}
