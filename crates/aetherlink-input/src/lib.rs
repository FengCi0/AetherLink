#![forbid(unsafe_code)]

use aetherlink_proto::v1::{InputEvent, input_event};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum InputCommand {
    Mouse {
        x: i32,
        y: i32,
        buttons_mask: u32,
    },
    Wheel {
        delta_x: i32,
        delta_y: i32,
    },
    Key {
        key_code: u32,
        down: bool,
        up: bool,
        repeat: bool,
        modifiers_mask: u32,
    },
    Touch {
        pointer_id: u32,
        x: f32,
        y: f32,
        down: bool,
        move_: bool,
        up: bool,
    },
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum InputError {
    #[error("missing input payload")]
    MissingPayload,
    #[error("invalid key event transition")]
    InvalidKeyTransition,
}

pub fn normalize_input_event(event: &InputEvent) -> Result<InputCommand, InputError> {
    match event.payload.as_ref() {
        Some(input_event::Payload::Mouse(mouse)) => Ok(InputCommand::Mouse {
            x: mouse.x,
            y: mouse.y,
            buttons_mask: mouse.buttons_mask,
        }),
        Some(input_event::Payload::Wheel(wheel)) => Ok(InputCommand::Wheel {
            delta_x: wheel.delta_x,
            delta_y: wheel.delta_y,
        }),
        Some(input_event::Payload::Key(key)) => {
            if key.down && key.up {
                return Err(InputError::InvalidKeyTransition);
            }
            Ok(InputCommand::Key {
                key_code: key.key_code,
                down: key.down,
                up: key.up,
                repeat: key.repeat,
                modifiers_mask: key.modifiers_mask,
            })
        }
        Some(input_event::Payload::Touch(touch)) => Ok(InputCommand::Touch {
            pointer_id: touch.pointer_id,
            x: touch.x,
            y: touch.y,
            down: touch.down,
            move_: touch.r#move,
            up: touch.up,
        }),
        None => Err(InputError::MissingPayload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aetherlink_proto::v1::{InputEvent, KeyEvent, input_event};

    #[test]
    fn rejects_invalid_key_transitions() {
        let event = InputEvent {
            session_id: "s".to_string(),
            source: 2,
            event_unix_ms: 0,
            payload: Some(input_event::Payload::Key(KeyEvent {
                key_code: 30,
                down: true,
                up: true,
                repeat: false,
                modifiers_mask: 0,
            })),
        };
        let err = normalize_input_event(&event).unwrap_err();
        assert_eq!(err, InputError::InvalidKeyTransition);
    }
}
