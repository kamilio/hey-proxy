use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct PartialCalls {
    active: Vec<(u64, Value)>,
    queue: VecDeque<(Option<u64>, Option<Value>)>,
    serial: u64,
    allocated_slots: usize,
    pub trace: Vec<Value>,
}
impl PartialCalls {
    pub fn pending(&self) -> bool {
        !self.active.is_empty()
    }
    fn drain(&mut self) -> Vec<Value> {
        let mut parts = Vec::new();
        while self.queue.front().is_some_and(|(_, value)| value.is_some()) {
            parts.push(self.queue.pop_front().unwrap().1.unwrap());
        }
        parts
    }
    pub fn part(&mut self, part: &Value) -> Result<Vec<Value>> {
        let Some(call) = part.get("functionCall") else {
            self.queue.push_back((None, Some(part.clone())));
            return Ok(self.drain());
        };
        let partial = call.get("partialArgs").is_some() || call["willContinue"] == true;
        let terminal = call.get("name").is_none()
            && call.get("args").is_none()
            && call.get("partialArgs").is_none()
            && call["willContinue"] != true;
        if !partial && !terminal {
            self.queue.push_back((None, Some(part.clone())));
            return Ok(self.drain());
        }
        if call.get("args").is_some() {
            bail!("Streaming function call cannot mix args and partialArgs");
        }
        self.trace.push(part.clone());
        if let Some(name) = call.get("name") {
            if let Some(id) = call.get("id")
                && self
                    .active
                    .iter()
                    .any(|(_, p)| p["functionCall"].get("id") == Some(id))
            {
                bail!("Duplicate active streaming function call ID");
            }
            let mut start = part.clone();
            start["functionCall"] = json!({"name":name,"args":{}});
            if let Some(id) = call.get("id") {
                start["functionCall"]["id"] = id.clone();
            }
            self.serial += 1;
            self.queue.push_back((Some(self.serial), None));
            self.active.push((self.serial, start));
        }
        let index = if let Some(id) = call.get("id") {
            self.active
                .iter()
                .position(|(_, p)| p["functionCall"].get("id") == Some(id))
                .ok_or_else(|| anyhow!("Unknown streaming function call ID"))?
        } else {
            self.active
                .len()
                .checked_sub(1)
                .ok_or_else(|| anyhow!("Function argument continuation has no active call"))?
        };
        let target = &mut self.active[index].1;
        if let Some(signature) = part.get("thoughtSignature") {
            if target
                .get("thoughtSignature")
                .is_some_and(|s| s != signature)
            {
                bail!("Conflicting streaming function signatures");
            }
            target["thoughtSignature"] = signature.clone();
        }
        let mut continuing = call["willContinue"] == true;
        if let Some(args) = call.get("partialArgs") {
            for arg in args
                .as_array()
                .ok_or_else(|| anyhow!("partialArgs must be an array"))?
            {
                let path = arg["jsonPath"]
                    .as_str()
                    .ok_or_else(|| anyhow!("partialArgs.jsonPath must be text"))?;
                let value = if let Some(v) = arg.get("stringValue").filter(|v| !v.is_null()) {
                    if !v.is_string() {
                        bail!("Invalid stringValue");
                    }
                    v.clone()
                } else if let Some(v) = arg.get("numberValue").filter(|v| !v.is_null()) {
                    if !v.is_number() {
                        bail!("Invalid numberValue");
                    }
                    v.clone()
                } else if let Some(v) = arg.get("boolValue").filter(|v| !v.is_null()) {
                    if !v.is_boolean() {
                        bail!("Invalid boolValue");
                    }
                    v.clone()
                } else if arg.get("nullValue").is_some() {
                    Value::Null
                } else {
                    bail!("Partial function argument has no value");
                };
                let segments = parse_path(path)?;
                set_path(
                    &mut target["functionCall"]["args"],
                    &segments,
                    value,
                    &mut self.allocated_slots,
                )?;
                continuing |= arg["willContinue"] == true;
            }
        }
        if !continuing {
            let (serial, completed) = self.active.remove(index);
            let slot = self
                .queue
                .iter_mut()
                .find(|(id, _)| *id == Some(serial))
                .ok_or_else(|| anyhow!("Missing partial function slot"))?;
            slot.1 = Some(completed);
        }
        Ok(self.drain())
    }
}
#[derive(Debug)]
enum Segment {
    Key(String),
    Index(usize),
}
fn parse_path(path: &str) -> Result<Vec<Segment>> {
    let mut chars = path
        .strip_prefix('$')
        .ok_or_else(|| anyhow!("Function JSON path must start with $"))?
        .chars()
        .peekable();
    let mut segments = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            '.' => {
                let mut name = String::new();
                while chars.peek().is_some_and(|c| *c != '.' && *c != '[') {
                    name.push(chars.next().unwrap());
                }
                if name.is_empty() {
                    bail!("Empty function JSON path key");
                }
                segments.push(Segment::Key(name));
            }
            '[' => {
                if chars.peek().is_some_and(|c| *c == '\'' || *c == '"') {
                    let quote = chars.next().unwrap();
                    let mut key = String::new();
                    let mut closed = false;
                    while let Some(c) = chars.next() {
                        if c == quote {
                            closed = true;
                            break;
                        }
                        if c == '\\' {
                            key.push(
                                chars
                                    .next()
                                    .ok_or_else(|| anyhow!("Incomplete escaped path key"))?,
                            );
                        } else {
                            key.push(c);
                        }
                    }
                    if !closed || chars.next() != Some(']') {
                        bail!("Invalid quoted function JSON path");
                    }
                    segments.push(Segment::Key(key));
                } else {
                    let mut number = String::new();
                    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                        number.push(chars.next().unwrap());
                    }
                    if chars.next() != Some(']') {
                        bail!("Invalid array function JSON path");
                    }
                    let index: usize = number.parse()?;
                    if index > 65535 {
                        bail!("Function argument array index exceeds 65535");
                    }
                    segments.push(Segment::Index(index));
                }
            }
            _ => bail!("Invalid function JSON path"),
        }
        if segments.len() > 128 {
            bail!("Function JSON path nesting exceeds 128");
        }
    }
    if segments.is_empty() {
        bail!("Function partialArgs must address a field, not the root");
    }
    Ok(segments)
}
fn set_path(
    target: &mut Value,
    path: &[Segment],
    value: Value,
    allocated_slots: &mut usize,
) -> Result<()> {
    if path.is_empty() {
        if let (Value::String(previous), Value::String(fragment)) = (&mut *target, &value) {
            previous.push_str(fragment);
        } else if target.is_null() {
            *target = value;
        } else {
            bail!("Streaming function argument overwrites a completed value");
        }
        return Ok(());
    }
    match &path[0] {
        Segment::Key(key) => {
            if target.is_null() {
                *target = json!({});
            }
            let object = target
                .as_object_mut()
                .ok_or_else(|| anyhow!("Function path expects an object"))?;
            set_path(
                object.entry(key.clone()).or_insert(Value::Null),
                &path[1..],
                value,
                allocated_slots,
            )
        }
        Segment::Index(index) => {
            if target.is_null() {
                *target = json!([]);
            }
            let array = target
                .as_array_mut()
                .ok_or_else(|| anyhow!("Function path expects an array"))?;
            if array.len() <= *index {
                let added = index + 1 - array.len();
                if added > 262144 - *allocated_slots {
                    bail!("Streaming function arrays exceed 262144 allocated slots");
                }
                *allocated_slots += added;
                array.resize(index + 1, Value::Null);
            }
            set_path(&mut array[*index], &path[1..], value, allocated_slots)
        }
    }
}
