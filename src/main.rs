use serde_json::{self, json};
use std::env;

#[allow(dead_code)]
fn decode_bencoded_value(encoded_value: &str) -> (serde_json::Value, &str) {
    let begin_delimiter = encoded_value.chars().next();

    match begin_delimiter {
        Some('0'..='9') => {
            let colon_index = encoded_value.find(':').unwrap();
            let number_string = &encoded_value[..colon_index];
            let number = number_string.parse::<i64>().unwrap();
            let string = &encoded_value[colon_index + 1..colon_index + 1 + number as usize];
            let rest = &encoded_value[colon_index + 1 + number as usize..];
            (serde_json::Value::String(string.to_string()), rest)
        }
        Some('i') => {
            let (mut n, rest) = encoded_value.split_at(encoded_value.find('e').unwrap());
            n = &n[1..];
            let number = n.parse::<i64>().unwrap();
            (json!(number), rest)
        }
        Some('l') => {
            let mut values = Vec::new();
            let mut rest = encoded_value.split_at(1).1;
            while !rest.is_empty() && !rest.starts_with('e') {
                let (v, remainder) = decode_bencoded_value(rest);
                values.push(v);
                rest = remainder;
            }
            return (values.into(), &rest[1..]);
        }
        Some('d') => {
            let mut dict = serde_json::Map::new();
            let mut rest = encoded_value.split_at(1).1;
            while !rest.is_empty() && !rest.starts_with('e') {
                let (k, remainder) = decode_bencoded_value(rest);
                let k = match k {
                    serde_json::Value::String(k) => k,
                    k => {
                        panic!("dict keys must be strings, not {k:?}");
                    }
                };
                let (v, remainder) = decode_bencoded_value(remainder);
                dict.insert(k, v);
                rest = remainder;
            }
            return (dict.into(), &rest[1..]);
        }
        _ => {
            panic!("Unhandled encoded value: {}", encoded_value)
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let command = &args[1];

    if command == "decode" {
        let encoded_value = &args[2];
        let (decoded_value, _) = decode_bencoded_value(encoded_value);
        println!("{}", decoded_value.to_string());
    } else {
        println!("unknown command: {}", args[1])
    }
}
