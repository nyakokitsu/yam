//! Permission-gated speech/state bridge for binary packages. No volume protocol:
//! that belongs to the existing station-volume package, not a second YAM copy.
use super::*;
use std::io::ErrorKind;

#[derive(Default)]
pub struct SpeechReader {
    stream: Option<TcpStream>,
    buffered: Vec<u8>,
    touched: Option<Instant>,
    initial: bool,
    phase: Option<String>,
}
impl SpeechReader {
    pub fn next(&mut self) -> Result<Option<serde_json::Value>> {
        if self
            .touched
            .is_some_and(|at| at.elapsed() > Duration::from_secs(3))
        {
            self.stream = None;
            self.buffered.clear();
            self.phase = None;
        }
        self.touched = Some(Instant::now());
        let result = self.read();
        if result.is_err() {
            self.stream = None;
            self.buffered.clear();
            self.phase = None;
        }
        match result {
            Ok(None) => Ok(self.phase.as_ref().map(|phase| {
                serde_json::json!({"phase":phase,
                "request_id":"","transcript":"","final":false,"initial_snapshot":false})
            })),
            other => other,
        }
    }
    fn read(&mut self) -> Result<Option<serde_json::Value>> {
        let deadline = Instant::now() + Duration::from_millis(800);
        if self.stream.is_none() {
            self.stream = connect_ipc(ipc_endpoint("aliced")?, deadline)?;
            self.initial = true;
        }
        let Some(stream) = self.stream.as_mut() else {
            return Ok(None);
        };
        loop {
            if self.buffered.len() >= 4 {
                let size = ipc_frame_length(self.buffered[..4].try_into()?)?;
                if self.buffered.len() >= size + 4 {
                    let frame = self.buffered[4..size + 4].to_vec();
                    self.buffered.drain(..size + 4);
                    let initial = self.initial;
                    self.initial = false;
                    if let Some(mut event) = speech_event(&frame)? {
                        if event["phase"] != "recognition" {
                            self.phase = event["phase"].as_str().map(str::to_owned);
                        }
                        event["initial_snapshot"] = serde_json::json!(initial);
                        return Ok(Some(event));
                    }
                    continue;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            stream.set_read_timeout(Some(left))?;
            let mut bytes = [0; 4096];
            match stream.read(&mut bytes) {
                Ok(0) => bail!("speech IPC disconnected"),
                Ok(n) => self.buffered.extend_from_slice(&bytes[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                    return Ok(None)
                }
                Err(e) => return Err(e.into()),
            }
            if self.buffered.len() > IPC_MAX_FRAME_SIZE + 4 {
                bail!("speech IPC buffer exceeded");
            }
        }
    }
}

fn speech_event(frame: &[u8]) -> Result<Option<serde_json::Value>> {
    let frame = normalize_ipc_message(frame)?;
    let Some(state) = nested_field(&frame, 3)? else {
        return Ok(None);
    };
    // Existing public schema, not a guessed mapping:
    // v3rt3p/quasar-protos/yandex_io/protos/alice_state.proto, State field1.
    let phase = match varint_field(&state, 1)? {
        Some(0) => "idle",
        Some(1) => "listening",
        Some(2) => "speaking",
        Some(3) => "busy",
        Some(5) => "shazam",
        Some(6) => "unknown",
        Some(_) => "unknown",
        None => "recognition",
    };
    let request_id = text_field(&state, 7)?.unwrap_or_default();
    let (transcript, final_result) = match nested_field(&state, 15)? {
        Some(asr) => (
            text_field(&asr, 1)?.unwrap_or_default(),
            varint_field(&asr, 2)? == Some(1),
        ),
        None => (String::new(), false),
    };
    if transcript.len() > 4096 {
        bail!("speech transcript exceeds limit");
    }
    // Do not expose response text, cards, account info or arbitrary raw frames.
    Ok(Some(
        serde_json::json!({"phase":phase,"request_id":request_id,
        "transcript":transcript,"final":final_result}),
    ))
}

pub async fn capabilities(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    runtime_permission(&a, &id, &headers, "exec").await?;
    let manifest = a.plugins.lock().await.manifest(&id)?;
    let has = |name: &str| manifest.permissions.iter().any(|p| p == name);
    let led_count = a.led.settings.lock().await.count;
    Ok(Json(
        serde_json::json!({"version":1,"speech":has("ipc.speech"),
        "commands":has("commands"),"led":has("led"),"led_count":led_count}),
    ))
}
pub async fn speech(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<Option<serde_json::Value>>, ApiError> {
    runtime_permission(&a, &id, &headers, "ipc.speech").await?;
    let token = headers
        .get("x-yam-token")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let reader = {
        let mut readers = a.speech_readers.lock().await;
        let entry = readers.entry(id.clone()).or_insert_with(|| {
            (
                token.clone(),
                Arc::new(std::sync::Mutex::new(SpeechReader::default())),
            )
        });
        if entry.0 != token {
            *entry = (
                token.clone(),
                Arc::new(std::sync::Mutex::new(SpeechReader::default())),
            );
        }
        entry.1.clone()
    };
    let event = task::spawn_blocking(move || {
        let mut guard = reader
            .try_lock()
            .map_err(|_| anyhow!("speech poll already in flight"))?;
        guard.next()
    })
    .await?;
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            a.speech_claims.lock().await.remove(&id);
            return Err(ApiError(error));
        }
    };
    if let Some(ref event) = event {
        let mut claims = a.speech_claims.lock().await;
        let request = event["request_id"].as_str().unwrap_or("");
        if claims.get(&id).is_some_and(|old| old.0 != token) {
            claims.remove(&id);
        }
        if !request.is_empty()
            && (event["initial_snapshot"] == true
                || !claims.get(&id).is_some_and(|old| old.1 == request))
        {
            claims.insert(
                id.clone(),
                (
                    token.clone(),
                    request.to_owned(),
                    String::new(),
                    Instant::now(),
                    event["initial_snapshot"] == true,
                ),
            );
        }
        if let Some(old) = claims.get_mut(&id) {
            if matches!(event["phase"].as_str(), Some("idle" | "unknown")) {
                old.4 = true;
                old.2.clear();
            } else if event["final"] == true && old.1 == request && !old.4 && old.2.is_empty() {
                old.2 = event["transcript"].as_str().unwrap_or("").to_owned();
                old.3 = Instant::now();
            }
        }
    }
    Ok(Json(event))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    request_id: String,
}
pub async fn cancel_command(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(claim): Json<Claim>,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "commands").await?;
    runtime_permission(&a, &id, &headers, "ipc.speech").await?;
    let policy = a.plugins.lock().await.manifest(&id)?.commands.phrases;
    let observed = {
        let mut claims = a.speech_claims.lock().await;
        let record = claims.get_mut(&id).context("no unclaimed final speech")?;
        let observed = record.clone();
        record.4 = true;
        record.2.clear();
        observed
    };
    let token = headers.get("x-yam-token").unwrap().to_str().unwrap();
    if observed.4
        || observed.0 != token
        || observed.1 != claim.request_id
        || observed.3.elapsed() > Duration::from_millis(1500)
        || !policy
            .iter()
            .any(|phrase| command_matches(phrase, &observed.2))
    {
        return Err(ApiError(anyhow!("speech claim expired or not declared")));
    }
    // Reuse YAM's existing fixed CANCEL_DIALOG implementation, not raw IPC.
    task::spawn_blocking(cancel_alice_dialog).await??;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_states_final_and_private_reply() {
        for (state, phase) in [
            (0, "idle"),
            (1, "listening"),
            (2, "speaking"),
            (3, "busy"),
            (5, "shazam"),
            (6, "unknown"),
            (99, "unknown"),
        ] {
            let mut body = vec![8, state];
            let mut asr = Vec::new();
            push_bytes(&mut asr, 1, "следующий трек".as_bytes());
            asr.extend_from_slice(&[16, 1]);
            push_bytes(&mut body, 15, &asr);
            push_bytes(&mut body, 7, b"request-1");
            push_bytes(&mut body, 14, b"private reply");
            let mut frame = Vec::new();
            push_bytes(&mut frame, 3, &body);
            let event = speech_event(&frame).unwrap().unwrap();
            assert_eq!(event["phase"], phase);
            assert_eq!(event["final"], true);
            assert!(!event.to_string().contains("private reply"));
        }
        assert!(speech_event(&[255]).is_err());
    }
}
