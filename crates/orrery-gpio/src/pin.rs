//! Moonraker's pin syntax: `^` pull-up or `~` pull-down, `!` inverted, an optional
//! `gpiochipN/`, then `gpioN`. Examples: `gpio26`, `!gpio72`, `^!gpiochip0/gpio27`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bias {
    /// The pin's bias isn't set: `^` and `~` aren't given.
    Default,
    PullUp,
    PullDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// The chip's device name, such as `gpiochip0`.
    pub chip: String,
    pub line: u32,
    /// `!`: the button is pressed when the line is low.
    pub inverted: bool,
    pub bias: Bias,
}

impl std::fmt::Display for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} line {}", self.chip, self.line)
    }
}

pub fn parse(text: &str) -> Result<Pin, String> {
    let bad = || {
        format!(
            "pin: {text:?} isn't a GPIO line; write it as gpio26, with ^ or ~ for a pull-up or pull-down, ! to invert, and gpiochip1/ for another chip"
        )
    };
    let mut rest = text.trim();
    let bias = match rest.chars().next() {
        Some('^') => Bias::PullUp,
        Some('~') => Bias::PullDown,
        _ => Bias::Default,
    };
    if bias != Bias::Default {
        rest = &rest[1..];
    }
    let inverted = rest.starts_with('!');
    if inverted {
        rest = &rest[1..];
    }
    let (chip, name) = match rest.split_once('/') {
        Some((chip, name)) => (chip, name),
        None => ("gpiochip0", rest),
    };
    let number = |s: &str, prefix: &str| {
        s.strip_prefix(prefix)
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse::<u32>().ok())
    };
    number(chip, "gpiochip").ok_or_else(bad)?;
    let line = number(name, "gpio").ok_or_else(bad)?;
    Ok(Pin { chip: chip.to_owned(), line, inverted, bias })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moonrakers_examples() {
        let p = |chip: &str, line, inverted, bias| Pin { chip: chip.into(), line, inverted, bias };
        assert_eq!(parse("gpiochip0/gpio26"), Ok(p("gpiochip0", 26, false, Bias::Default)));
        assert_eq!(parse("gpio26"), Ok(p("gpiochip0", 26, false, Bias::Default)));
        assert_eq!(parse("!gpio72"), Ok(p("gpiochip0", 72, true, Bias::Default)));
        assert_eq!(parse("^gpiochip4/gpio12"), Ok(p("gpiochip4", 12, false, Bias::PullUp)));
        assert_eq!(parse("~!gpiochip0/gpio26"), Ok(p("gpiochip0", 26, true, Bias::PullDown)));
        assert_eq!(parse(" ^!gpio19 "), Ok(p("gpiochip0", 19, true, Bias::PullUp)));
    }

    #[test]
    fn not_pins() {
        for bad in ["", "26", "gpio", "gpioA", "!^gpio1", "PA10", "gpiochip/gpio1", "chip0/gpio1", "gpio1/gpio2"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
