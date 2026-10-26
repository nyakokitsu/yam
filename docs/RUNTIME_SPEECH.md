# Binary speech/state API (local proposal, not stock YAM)

Reuses YAM's existing loopback endpoint, envelope/protobuf helpers, authorization,
command policy and fixed cancel_alice_dialog function. No native volume API:
volume stays in the existing station-volume dependency.

- GET /api/runtime/ID/media: version1, declared speech/commands/led permission,
  configured led_count. Requires current runtime token and exec permission.
- POST /api/runtime/ID/speech/next: requires ipc.speech. Up to800ms polling on a
  persistent per-invocation aliced stream, preserved partial headers/payloads,
  bounded frame size, no background log reading or transcript recording.
- Phase names derive from public quasar-protos AliceState field1: idle0,
  listening1, speaking2, busy3, shazam5; NONE/unknown is not treated as idle.
- Final ASR is RequestState15/is_final2, with request_id7. ResponseState14 is NOT
  assumed to mean speaking. Assistant replies/cards/account fields are excluded.
- Initial snapshot is marked: never execute it as a new voice command. Polling
  timeouts may carry the last phase without any transcript, while the stream is
  connected. After inactivity>3s, reconnect and mark a new initial snapshot.
- POST /api/runtime/ID/command/cancel with request_id requires commands and
  ipc.speech, the current token, a fresh (<1500ms), unclaimed final delivered to
  that invocation and an exact declared [commands].phrases match. Reuses the
  existing fixed CANCEL_DIALOG envelope, not an arbitrary raw IPC write.
  Initial/idle/unknown events and repeated claims are rejected. Native cancellation
  is not transactionally correlated on the wire: hardware race/latency tests remain.

Runtime token rotation and service exit discard stream/claims. A missing stream
is unavailable, not inferred Alice IDLE. User transcripts stay in bounded RAM
only while necessary for the claim; do not include them in logs or diagnostics.

Validation: Rust schema tests, isolated real HTTP token/permission/fragmentation/
claim fixture, room regressions and AArch64 musl cross-build. No actual Quasar
firmware, live speaker speech, native LEDs or audio are involved in these tests.
