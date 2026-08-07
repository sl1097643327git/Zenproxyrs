use std::collections::VecDeque;

pub struct SseBuffer {
    buf: VecDeque<u8>,
    /// 跨 `push_bytes` 调用保留的未完成行（前一个 chunk 末尾没遇到换行的字节）。
    /// 此前是 `push_bytes` 的局部变量，导致每个 chunk 边界都会丢失行首字节，
    /// 产生无 `data:` 前缀的裸行碎片（且真实数据被静默丢弃）。
    pending_line: Vec<u8>,
    done: bool,
}

impl SseBuffer {
    pub fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            pending_line: Vec::new(),
            done: false,
        }
    }

    pub fn done(&self) -> bool {
        self.done
    }

    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        for &b in chunk {
            self.buf.push_back(b);
        }
        let mut lines: Vec<Vec<u8>> = Vec::new();
        // 从头一个 chunk 保留的未完成行继续累积，避免跨 chunk 丢字节。
        let mut current_line: Vec<u8> = std::mem::take(&mut self.pending_line);
        while let Some(&b) = self.buf.front() {
            if b == b'\n' || b == b'\r' {
                let _ = self.buf.pop_front();
                if b == b'\r' && self.buf.front() == Some(&b'\n') {
                    let _ = self.buf.pop_front();
                }
                if self.done {
                    current_line.clear();
                    continue;
                }
                if current_line == b"data: [DONE]" || current_line == b"data:[DONE]" {
                    self.done = true;
                    lines.push(b"data: [DONE]\n".to_vec());
                } else if current_line.is_empty() {
                    lines.push(vec![b'\n']);
                } else {
                    let patched = patch_sse_line(&current_line);
                    if !patched.is_empty() {
                        let mut line_with_nl = patched;
                        line_with_nl.push(b'\n');
                        lines.push(line_with_nl);
                    }
                }
                current_line.clear();
            } else {
                current_line.push(b);
                let _ = self.buf.pop_front();
            }
        }
        // 未遇到换行的剩余字节保留到下一次调用继续累积。
        self.pending_line = current_line;
        lines
    }
}

fn patch_sse_line(line: &[u8]) -> Vec<u8> {
    if line.is_empty() || line.starts_with(b":") {
        return line.to_vec();
    }
    if !line.starts_with(b"data") {
        return line.to_vec();
    }
    if line == b"data: [DONE]" || line == b"data:[DONE]" {
        return b"data: [DONE]".to_vec();
    }
    let (payload, has_space) = if line.starts_with(b"data: ") {
        (&line[6..], true)
    } else if line.starts_with(b"data:") {
        (&line[5..], false)
    } else {
        return line.to_vec();
    };
    if payload.is_empty() {
        return line.to_vec();
    }
    let mut val: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => return line.to_vec(),
    };
    if let Some(obj) = val.as_object_mut() {
        if let Some(choices) = obj.get_mut("choices") {
            if let Some(choices_arr) = choices.as_array_mut() {
                for choice in choices_arr.iter_mut() {
                    let delta = if choice.get("delta").is_some() {
                        choice.get_mut("delta")
                    } else {
                        choice.get_mut("message")
                    };
                    if let Some(delta_val) = delta {
                        if let Some(delta_obj) = delta_val.as_object_mut() {
                            let content = delta_obj.get("content").and_then(|v| v.as_str());
                            let reasoning =
                                delta_obj.get("reasoning_content").and_then(|v| v.as_str());
                            let has_reasoning = reasoning.is_some_and(|r| !r.is_empty());
                            let has_content = content.is_some_and(|c| !c.is_empty());
                            if has_reasoning && !has_content {
                                // 推理阶段：content 显式置 null，让下游知道正文尚未开始。
                                delta_obj.insert(
                                    "content".to_string(),
                                    serde_json::Value::Null,
                                );
                            } else if has_content && !has_reasoning {
                                // 回答阶段：reasoning_content 显式置 null。
                                delta_obj.insert(
                                    "reasoning_content".to_string(),
                                    serde_json::Value::Null,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    let mut out = if has_space {
        vec![b'd', b'a', b't', b'a', b':', b' ']
    } else {
        vec![b'd', b'a', b't', b'a', b':']
    };
    let _ = serde_json::to_writer(&mut out, &val);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_done_event() {
        let mut buf = SseBuffer::new();
        let result = buf.push_bytes(b"data: [DONE]\n\n");
        let done_count = result
            .iter()
            .filter(|r| r.windows(b"DONE".len()).any(|w| w == b"DONE"))
            .count();
        assert_eq!(done_count, 1);
    }

    #[test]
    fn sets_done_flag_after_done() {
        let mut buf = SseBuffer::new();
        buf.push_bytes(b"data: [DONE]\n");
        let lines = buf.push_bytes(b"\ndata: extra\n");
        assert!(
            lines.is_empty()
                || lines
                    .iter()
                    .all(|r| !r.windows(b"extra".len()).any(|w| w == b"extra"))
        );
    }

    #[test]
    fn reasoning_phase_nullifies_content() {
        // 推理阶段：reasoning_content 非空、content 空 → content 应显式置 null，
        // 而不是被复制成与 reasoning_content 同值的字符串。
        let mut buf = SseBuffer::new();
        let json = r#"data: {"choices":[{"index":0,"delta":{"content":null,"reasoning_content":"思考中"}}]}"#;
        let result = buf.push_bytes(format!("{}\n\n", json).as_bytes());
        let all: Vec<u8> = result.iter().flatten().copied().collect();
        let s = String::from_utf8_lossy(&all);
        assert!(s.contains("\"reasoning_content\":\"思考中\""));
        assert!(s.contains("\"content\":null"));
        // 不再把 reasoning 复制进 content，二者不得同值。
        assert!(!s.contains("\"content\":\"思考中\""));
        let reasoning_count = s.matches("\"reasoning_content\":\"思考中\"").count();
        let content_value_count = s.matches("\"content\":\"思考中\"").count();
        assert_eq!(reasoning_count, 1);
        assert_eq!(content_value_count, 0);
    }

    #[test]
    fn answer_phase_nullifies_reasoning() {
        // 回答阶段：content 非空、reasoning_content 空 → reasoning_content 应显式置 null。
        let mut buf = SseBuffer::new();
        let json = r#"data: {"choices":[{"index":0,"delta":{"content":"正文","reasoning_content":null}}]}"#;
        let result = buf.push_bytes(format!("{}\n\n", json).as_bytes());
        let all: Vec<u8> = result.iter().flatten().copied().collect();
        let s = String::from_utf8_lossy(&all);
        assert!(s.contains("\"content\":\"正文\""));
        assert!(s.contains("\"reasoning_content\":null"));
    }

    #[test]
    fn handles_chunk_split_inside_json() {
        let mut buf = SseBuffer::new();
        let line = r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}]}"#;
        let mid = line.len() / 2;
        let first_part = &line[..mid];
        let second_part = &line[mid..];
        buf.push_bytes(first_part.as_bytes());
        let result = buf.push_bytes(format!("{}\n\n", second_part).as_bytes());
        let all: Vec<u8> = result.iter().flatten().copied().collect();
        let s = String::from_utf8_lossy(&all);
        assert!(s.contains("content"));
    }

    #[test]
    fn no_bare_lines_across_chunk_boundaries() {
        // 回归测试：旧 bug 的 current_line 是局部变量，每个 chunk 末尾未完成行
        // 被丢弃，导致下一个 chunk 从行中间开始，输出无 `data:` 前缀的裸行碎片。
        // 用小 chunk 刻意把事件切断在 JSON 中间。
        let event1 = r#"data: {"choices":[{"index":0,"delta":{"content":"第一段"}}]}"#;
        let event2 = r#"data: {"choices":[{"index":0,"delta":{"content":"第二段"}}]}"#;
        let stream = format!("{}\n\n{}\n\ndata: [DONE]\n\n", event1, event2);
        let bytes: Vec<u8> = stream.into_bytes();
        let mut buf = SseBuffer::new();
        let mut all: Vec<u8> = Vec::new();
        let step = 7;
        let mut i = 0;
        while i < bytes.len() {
            let end = (i + step).min(bytes.len());
            for line in buf.push_bytes(&bytes[i..end]) {
                all.extend_from_slice(&line);
            }
            i = end;
        }
        let s = String::from_utf8_lossy(&all);
        for l in s.split('\n') {
            if l.is_empty() || l.starts_with("data") {
                continue;
            }
            panic!("bare line leaked through chunk boundary: {:?}", l);
        }
        // 两个事件的内容都必须完整保留（跨 chunk 不能丢字节）。
        assert!(s.contains("\"content\":\"第一段\""), "event 1 content missing");
        assert!(s.contains("\"content\":\"第二段\""), "event 2 content missing");
    }

    #[test]
    fn handles_multiple_events_in_one_chunk() {
        let mut buf = SseBuffer::new();
        let chunk = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"}}]}\n\n";
        let result = buf.push_bytes(chunk);
        // We expect at least 2 data lines (the JSON payloads), plus blank-line separators;
        // total is 4: [data-with-a\n, \n, data-with-b\n, \n]; or 2 filtered payloads.
        // Test that we got multiple payloads, not just one:
        let payload_count = result.iter().filter(|r| r.starts_with(b"data")).count();
        assert!(
            payload_count >= 2,
            "got {} data lines, expected >= 2",
            payload_count
        );
    }

    #[test]
    fn preserves_comments_and_blank_event_boundaries() {
        let mut buf = SseBuffer::new();
        let result = buf.push_bytes(b": keep-alive\r\n\r\ndata: {\"ok\":true}\r\n\r\n");
        let all: Vec<u8> = result.iter().flatten().copied().collect();
        let s = String::from_utf8_lossy(&all);
        assert!(s.contains(": keep-alive\n\n"));
        assert!(s.contains("data: {\"ok\":true}\n\n"));
    }

    #[test]
    fn drops_after_done_tail_events() {
        let mut buf = SseBuffer::new();
        buf.push_bytes(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\n");
        buf.push_bytes(b"data: [DONE]\n\n");
        let result = buf.push_bytes(b"data: {\"choices\":[],\"cost\":\"0\"}\n\n");
        let all: Vec<u8> = result.iter().flatten().copied().collect();
        let s = String::from_utf8_lossy(&all);
        assert!(!s.contains("cost"), "should drop after DONE");
    }
}
