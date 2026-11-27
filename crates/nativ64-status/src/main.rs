//! Prints the status file of the "nativ64" tab for tools/import_map.py: every export the host
//! API library and the graphics modules register, with its status. Both crates are needed, so
//! the tool lives in its own small crate (the graphics modules depend on the library).

use std::collections::BTreeMap;

/// Every registered export, `module!function -> status`; a module registered by both crates
/// keeps the library's answer (they do not overlap today).
fn statuses() -> BTreeMap<String, &'static str> {
    let mut map = BTreeMap::new();
    for e in nativ64::exports().chain(nativ64_gpu::exports()) {
        map.entry(format!("{}!{}", e.module, e.name))
            .or_insert_with(|| e.status.as_str());
    }
    map
}

fn main() {
    let map = statuses();
    println!("{{");
    println!("  \"tab\": \"nativ64\",");
    println!("  \"title\": \"nativ64: the host API library and graphics modules\",");
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
    let keys: Vec<String> = map
        .iter()
        .map(|(k, v)| format!("    \"{k}\": \"{v}\""))
        .collect();
    println!("{}", keys.join(",\n"));
    println!("  }}");
    println!("}}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_crates_are_counted() {
        let m = statuses();
        assert_eq!(m.get("kernel32!GetLastError"), Some(&"native"));
        assert_eq!(
            m.get("d3d12_xs!D3D12XboxCreateDevice"),
            Some(&"native"),
            "a graphics export is part of the nativ64 tab"
        );
        assert_eq!(m.get("xmem!XMemFree"), Some(&"native"));
        assert_eq!(m.get("xg_xs!XGSuggestSwizzleMode"), Some(&"native"));
    }

    #[test]
    fn every_registration_is_in_the_map_with_one_status() {
        let m = statuses();
        let mut seen = 0;
        for e in nativ64::exports().chain(nativ64_gpu::exports()) {
            let key = format!("{}!{}", e.module, e.name);
            assert_eq!(
                m.get(&key),
                Some(&e.status.as_str()),
                "{key} is registered twice with a different status"
            );
            assert_eq!(key.matches('!').count(), 1, "{key}");
            seen += 1;
        }
        assert!(seen > 900, "only {seen} registrations");
    }
}
