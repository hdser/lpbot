//! The keystore's password, typed by the owner when the signer starts. It
//! is read from the terminal without echo, never from an environment
//! variable, a file or a pipe, so nothing a session or a script can read
//! holds it.

use thiserror::Error;
use zeroize::Zeroizing;

#[derive(Debug, Error)]
pub enum PasswordError {
    #[error("the signer reads the keystore's password from a terminal and found none ({0}): start `lpbot signer` in a terminal, such as a tmux window on the node, not from cron, a service or a pipe")]
    NoTerminal(String),
    #[error("no password was typed: start the signer again and type the keystore's password")]
    Empty,
}

/// Asks for the password on the terminal, without echo.
pub fn read_password(prompt: &str) -> Result<Zeroizing<String>, PasswordError> {
    let typed =
        rpassword::prompt_password(prompt).map_err(|e| PasswordError::NoTerminal(e.to_string()))?;
    checked(Zeroizing::new(typed))
}

/// The password from a reader, through the same reading as the terminal:
/// for tests.
#[cfg(test)]
fn password_from(reader: impl std::io::Read + 'static) -> Result<Zeroizing<String>, PasswordError> {
    let config = rpassword::ConfigBuilder::new()
        .input_reader(reader)
        .output_discard()
        .build();
    let typed = rpassword::read_password_with_config(config)
        .map_err(|e| PasswordError::NoTerminal(e.to_string()))?;
    checked(Zeroizing::new(typed))
}

fn checked(typed: Zeroizing<String>) -> Result<Zeroizing<String>, PasswordError> {
    if typed.is_empty() {
        return Err(PasswordError::Empty);
    }
    Ok(typed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_typed_password_is_read_to_the_end_of_its_line() {
        let typed = password_from(Cursor::new("correct horse battery\n")).unwrap();
        assert_eq!(typed.as_str(), "correct horse battery");
        assert!(matches!(
            password_from(Cursor::new("\n")),
            Err(PasswordError::Empty)
        ));
    }

    #[test]
    fn without_a_terminal_the_error_says_where_to_start_the_signer() {
        let message = PasswordError::NoTerminal("no tty".into()).to_string();
        assert!(message.contains("lpbot signer"));
        assert!(message.contains("terminal"));
    }
}
