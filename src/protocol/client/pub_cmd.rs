use bytes::{BufMut, Bytes, BytesMut};
use protocol::{Command, CommandError};
use rand::{distributions::Alphanumeric, thread_rng, Rng};

#[derive(Debug, Clone, Builder)]
#[builder(build_fn(validate = "Self::validate"))]
pub struct PubCommand {
    pub subject: String,
    #[builder(default)]
    pub reply_to: Option<String>,
    pub payload: Bytes,
}

impl Command for PubCommand {
    const CMD_NAME: &'static [u8] = b"PUB";

    fn into_vec(self) -> Result<Bytes, CommandError> {
        let rt = if let Some(reply_to) = self.reply_to {
            format!("\t{}", reply_to)
        } else {
            "".into()
        };

        let cmd_str = format!("PUB\t{}{}\t{}\r\n", self.subject, rt, self.payload.len());
        let mut bytes = BytesMut::new();
        bytes.put(cmd_str.as_bytes());
        bytes.put(self.payload);
        bytes.put("\r\n");

        Ok(bytes.freeze())
    }

    fn try_parse(buf: &[u8]) -> Result<Self, CommandError> {
        let len = buf.len();

        if buf[len - 2..] != [b'\r', b'\n'] {
            return Err(CommandError::IncompleteCommandError);
        }

        if let Some(payload_start) = buf[..len - 3].iter().position(|b| *b == b'\r') {
            if buf[payload_start + 1] != b'\n' {
                return Err(CommandError::CommandMalformed);
            }

            let payload: Bytes = buf[payload_start..len - 2].into();

            let whole_command = ::std::str::from_utf8(&buf[..payload_start])?;
            let mut split = whole_command.split_whitespace();
            let cmd = split.next().ok_or_else(|| CommandError::CommandMalformed)?;
            // Check if we're still on the right command
            if cmd.as_bytes() != Self::CMD_NAME {
                return Err(CommandError::CommandMalformed);
            }

            let payload_len: usize = split
                .next_back()
                .ok_or_else(|| CommandError::CommandMalformed)?
                .parse()?;

            if payload.len() != payload_len {
                return Err(CommandError::CommandMalformed);
            }

            // Extract subject
            let subject: String = split.next().ok_or_else(|| CommandError::CommandMalformed)?.into();

            let reply_to: Option<String> = split.next().map(|v| v.into());

            Ok(PubCommand {
                subject,
                payload,
                reply_to,
            })
        } else {
            Err(CommandError::CommandMalformed)
        }
    }
}

impl PubCommandBuilder {
    pub fn generate_reply_to() -> String {
        let mut rng = thread_rng();
        rng.sample_iter(&Alphanumeric).take(16).collect()
    }

    pub fn auto_reply_to(&mut self) -> &mut Self {
        let inbox = Self::generate_reply_to();
        self.reply_to = Some(Some(inbox));
        self
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(ref subj) = self.subject {
            check_cmd_arg!(subj, "subject");
        }

        if let Some(ref reply_to_maybe) = self.reply_to {
            if let Some(ref reply_to) = reply_to_maybe {
                check_cmd_arg!(reply_to, "inbox");
            }
        }

        Ok(())
    }
}
