//! Neutral core for the `numwords` extension — spell numbers as
//! cardinal / ordinal words (via `num2words`) — written ONCE. The per-DB
//! shim is generated from the [`declare!`](datalink_extcore::declare) table.
//!
//!   * `num_to_words(n int64) -> text` (cardinal)
//!   * `num_to_ordinal_words(n int64) -> text` (ordinal)
//!
//! An unrepresentable number yields NULL; NULL args propagate.

extern crate alloc;

use datalink_extcore::NeutralValue;
use num2words::Num2Words;

datalink_extcore::declare! {
    core = Core;
    extension = "numwords";
    version = env!("CARGO_PKG_VERSION");

    scalar num_to_words(int64) -> text [propagate, deterministic] = |args| {
        let n = args.arg_int(0, "num_to_words")?;
        Ok(match Num2Words::new(n).cardinal().to_words() {
            Ok(s) => NeutralValue::Text(s),
            Err(_) => NeutralValue::Null,
        })
    };

    scalar num_to_ordinal_words(int64) -> text [propagate, deterministic] = |args| {
        let n = args.arg_int(0, "num_to_ordinal_words")?;
        Ok(match Num2Words::new(n).ordinal().to_words() {
            Ok(s) => NeutralValue::Text(s),
            Err(_) => NeutralValue::Null,
        })
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use datalink_extcore::ExtCore;
    use alloc::string::String;
    use std::vec;

    #[test]
    fn cardinal_and_ordinal() {
        assert_eq!(
            Core::dispatch(0, &[NeutralValue::Int64(123)]).unwrap(),
            NeutralValue::Text(String::from("one hundred twenty-three"))
        );
        assert_eq!(
            Core::dispatch(1, &[NeutralValue::Int64(21)]).unwrap(),
            NeutralValue::Text(String::from("twenty-first"))
        );
    }
}
