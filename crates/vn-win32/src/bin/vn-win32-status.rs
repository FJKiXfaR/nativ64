//! Prints the status file of the "vn-win32" tab for tools/import_map.py.

fn main() {
    let mut keys: Vec<String> = vn_win32::exports()
        .map(|e| format!("    \"{}!{}\": \"{}\"", e.module, e.name, e.status.as_str()))
        .collect();
    keys.sort();
    println!("{{");
    println!("  \"tab\": \"vn-win32\",");
    println!("  \"title\": \"vn-win32: the host API library\",");
    println!("  \"headline\": \"functions implemented by vn-win32\",");
    println!("  \"labels\": {{");
    println!("    \"native\": \"Native in vn-win32\",");
    println!("    \"stub\": \"Stub in vn-win32\",");
    println!("    \"crash\": \"Unresolved\",");
    println!("    \"todo\": \"Not in vn-win32 yet\",");
    println!("    \"shipped\": \"Shipped with the game\"");
    println!("  }},");
    println!("  \"done\": [\"native\"],");
    println!("  \"default\": \"todo\",");
    println!("  \"functions\": {{");
    println!("{}", keys.join(",\n"));
    println!("  }}");
    println!("}}");
}
