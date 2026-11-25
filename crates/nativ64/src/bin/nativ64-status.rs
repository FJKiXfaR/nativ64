//! Prints the status file of the "nativ64" tab for tools/import_map.py.

fn main() {
    let mut keys: Vec<String> = nativ64::exports()
        .map(|e| format!("    \"{}!{}\": \"{}\"", e.module, e.name, e.status.as_str()))
        .collect();
    keys.sort();
    println!("{{");
    println!("  \"tab\": \"nativ64\",");
    println!("  \"title\": \"nativ64: the host API library\",");
    println!("  \"headline\": \"functions implemented by nativ64\",");
    println!("  \"labels\": {{");
    println!("    \"native\": \"Native in nativ64\",");
    println!("    \"stub\": \"Stub in nativ64\",");
    println!("    \"crash\": \"Unresolved\",");
    println!("    \"todo\": \"Not in nativ64 yet\",");
    println!("    \"shipped\": \"Shipped with the game\"");
    println!("  }},");
    println!("  \"done\": [\"native\"],");
    println!("  \"default\": \"todo\",");
    println!("  \"functions\": {{");
    println!("{}", keys.join(",\n"));
    println!("  }}");
    println!("}}");
}
