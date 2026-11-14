# Way of Aiming — Specification (Draft 2)

A training aid that records how a 10 m air rifle moves on the target while aiming, detects the shot by sound, and shows the aim trace, the estimated shot position and stability metrics.

Status: draft 2 for review. Items marked **[open]** still need a decision.

## 1. Goals and scope

### Must have
- Track the aiming point on the target from the moment the target is detected until 2 s after the shot.
- Keep a few seconds of pre-roll from before the target was detected.
- Detect the shot from the webcam microphone, for both live fire and dry fire.
- Show the aiming point live while aiming, and the full trace right after each shot.
- Estimate shot position and score (decimal ring value).
- Calibrate after the fact: the shooter drags the displayed shot to where the pellet really hit.
- Stability metrics per shot and per session.
- Save every shot; sessions with history; shot comparison / overlay; multiple shooter profiles.
- Raw video recording as a debug feature that can be switched on and off.

### Nice to have (later)
- Electronic target integration: read the real shot coordinates from the target system and use them to calibrate automatically (see section 11).

### Out of scope for now
- Export (CSV / JSON / images).
- Disciplines other than 10 m air rifle.
- Cloud sync, accounts, remote coaching.

## 2. Physical setup

| Item | Value |
|---|---|
| Distance muzzle to target | 10 m |
| Camera | USB webcam (UVC) with tele / zoom lens, mounted a few centimetres below the barrel, pointing parallel to it |
| Microphone | The one built into the webcam |
| Host | Laptop or desktop PC (Windows, macOS, Linux), connected by USB cable |
| Target | Electronic target, or a single ISSF 10 m air rifle paper target |

### Camera requirements

| Property | Minimum | Preferred |
|---|---|---|
| Field of view at 10 m (horizontal) | 250–500 mm (about 1.4°–2.9°) | about 300 mm |
| Diameter of the black aiming mark in the image | 40 px | 100 px or more |
| Frame rate | 30 fps | 60 fps or more |
| Exposure | Manual, lockable, 1/250 s or shorter | 1/500 s |
| Focus | Manual, lockable at 10 m | |
| Auto white balance / auto gain | Can be switched off | |

Reasoning: the 30.5 mm black mark has to be large enough for sub-pixel centre finding, but the view has to be wide enough that the mark stays in the picture during the approach and the hold. Short exposure avoids motion blur, which would smear the centre estimate.

The mount must be rigid: any movement of the camera relative to the barrel shifts the calibration.

## 3. Reference geometry (ISSF 10 m air rifle target)

- Ring 10: 0.5 mm diameter. Each lower ring adds 5.0 mm diameter (ring 1 = 45.5 mm).
- Black aiming mark: rings 4 to 9, 30.5 mm diameter.
- Pellet diameter: 4.5 mm.
- Decimal score from the distance `r` (mm) between pellet centre and target centre:
  `score = 10.9 − floor(r / 0.25) × 0.1`, and 0 if `r ≥ 25.0`.
- Integer score is the decimal score rounded down.

All positions in the system are stored in millimetres on the target plane, origin at the target centre, x to the right, y up.

## 4. System overview

```
USB webcam ──video──▶ ┌──────────────────────────────┐
                      │ Local backend                │      ┌────────────┐
USB webcam ──audio──▶ │  capture → target tracking   │◀────▶│ Browser UI │
                      │  shot detection → recorder   │      └────────────┘
                      │  storage (local database)    │
                      └──────────────────────────────┘
```

- A local backend process owns the camera and microphone, does all processing and stores the data.
- The UI is a web page served by the backend. It opens in a browser on the same PC and can also be opened from a tablet on the same network.
- Live data (aim position, state, shot events) is pushed to the UI over a streaming connection (e.g. WebSocket).
- Everything runs offline. No data leaves the machine.
- The implementation language is left open; this document specifies behaviour and interfaces only.

## 5. Shot lifecycle

```
IDLE ──target found──▶ AIMING ──shot detected──▶ FOLLOW-THROUGH ──2 s──▶ SAVE ──▶ IDLE
  ▲                      │
  └──target lost > 2 s───┘  (attempt discarded)
```

| State | Behaviour |
|---|---|
| IDLE | Camera and microphone run. Video frames, audio and tracking results go into a ring buffer holding the last 3 s (configurable 1–10 s). |
| AIMING | Entered when the target has been detected in 5 consecutive frames. The ring buffer content becomes the pre-roll of the recording. Every frame's aim position is appended to the trace and pushed to the UI. |
| FOLLOW-THROUGH | Entered at the shot instant. Recording continues for 2 s (configurable 0–5 s). |
| SAVE | Trace, shot instant, score, metrics, audio clip and (if enabled) video are stored. The shot appears in the UI within 1 s. |

Rules:
- If the target is lost for less than 2 s during AIMING, the trace gets a gap and the attempt continues.
- If the target is lost for more than 2 s without a shot, the attempt is discarded by default. A setting allows keeping these as "aborted holds".
- A sound that is detected as a shot while the target is not in view is ignored.
- After a shot, a new attempt can only begin once FOLLOW-THROUGH has ended.
- The trace is kept for the whole aim regardless of length. Debug video is capped at 120 s per shot (oldest part dropped, pre-roll kept).

## 6. Target tracking

Input: one video frame. Output: aim position in mm, plus a confidence value.

1. Find the black aiming mark: a dark, round blob on a light background, within an expected size range.
2. Compute its centre with sub-pixel precision (e.g. edge-based ellipse fit) and its diameter in pixels.
3. Scale: `mm per pixel = aiming mark diameter in mm / measured diameter in px`. The aiming mark diameter comes from the target profile (30.5 mm for ISSF paper; configurable for electronic targets whose visible mark differs).
4. The aim position is where the optical axis reference point (image centre plus calibration offset) falls on the target plane, expressed relative to the target centre.
5. Camera roll (rifle cant or a tilted mount) is estimated automatically in every frame and corrected. The round aiming mark carries no roll information, so the estimate comes from straight edges around it: the edges of the paper card or the frame of the electronic target. A fixed mount roll per rifle profile is set once in a setup step (rifle held level) and subtracted, so that what remains is the cant of the rifle. If no usable edges are visible, the last good estimate is used and the samples are flagged as cant-unknown.

Requirements:
- Position noise with the rifle clamped still: 0.1 mm RMS or better.
- Processing keeps up with the camera frame rate; no frame dropping at 60 fps on a typical laptop.
- Works under ordinary indoor range lighting and with back-lit electronic targets. Exposure is locked after a setup step so brightness does not pump.
- Frames where the mark is cut off by the image edge, blurred, or not round enough are flagged low-confidence and excluded from metrics.

Target profiles hold: name, aiming mark diameter, ring dimensions, and detection hints (expected contrast, surrounding frame shape). Shipped profiles: ISSF 10 m air rifle paper target, and a generic electronic target profile with adjustable mark diameter.

## 7. Shot detection

The shot instant is taken from the audio. Because the microphone sits on the rifle, the own shot and even the dry-fire click arrive loud and with a sharp onset.

### Detection modes

| Mode | Use | Rule |
|---|---|---|
| Simple | Shooting alone | Audio onset: short-time energy rises above the running noise floor by a configurable margin. |
| Strict | Busy range | Audio onset as above **and** at least one confirmation: (a) the onset matches the learned loudness / spectral profile of the own rifle, or (b) the image shows a jolt (sudden aim displacement or blur) within ±50 ms of the onset. |

- A "teach" step records 3 shots (separately for live fire and dry fire) to learn the own rifle's sound level and character. Strict mode requires it.
- Dry fire produces almost no jolt, so in strict mode dry fire relies on the sound profile; live fire can use both.
- Minimum gap between two shots: 3 s.
- Manual correction in the UI: delete a false shot, or mark a missed shot on a kept recording by picking the instant on the timeline.

### Timing
- Audio and video are timestamped on a common clock at capture.
- A configurable audio-video offset (per camera, default 0 ms) compensates for device latency. A calibration helper estimates it from the image jolt of a few live shots.
- The aim position at the shot instant is interpolated between the neighbouring frames.
- A "shot delay" models the time between sound onset and the pellet leaving the barrel. Default 0 ms, adjustable per rifle profile.

## 8. Calibration

The camera sits below the barrel and is never perfectly parallel to it, so the raw aim position has a constant offset from the true point of impact at 10 m.

- **Scale** is automatic (section 6).
- **Offset** is set after the fact: in the shot view the shooter drags the shot marker to where the pellet really hit (read from the paper or the electronic target monitor). The difference is stored as the calibration offset.
- The offset can be applied to: this shot only, this shot and all following shots (default), or the whole session.
- Applying an offset shifts the whole trace of the affected shots and recalculates score and metrics. Raw, uncalibrated data is always kept so calibration can be redone.
- The UI also accepts the real hit as typed values (decimal score plus clock direction, or x / y in mm).
- The current offset is stored per shooter and rifle profile and is the starting value for the next session.
- Dry-fire shots use the current offset as is.

## 9. Metrics

Per shot (calculated on calibrated, high-confidence samples):

| Metric | Definition |
|---|---|
| Score | Decimal and integer ring value at the shot instant |
| Aim time | Time from target detection to shot |
| Hold steadiness | Mean distance of the trace from its own centre during the last 1.0 s before the shot (mm) |
| 10.0 % / 10.5 % | Share of the last 1.0 s in which the aim would have scored at least 10.0 / 10.5 |
| Trace speed | Trace length during the last 1.0 s (mm/s) |
| Release deviation | Distance between the mean aim position 0.6–0.2 s before the shot and the shot position (mm) — shows trigger errors |
| Hold centre offset | Distance and direction of the mean aim position in the last 1.0 s from the target centre |
| Follow-through | Largest displacement in the 0.5 s after the shot (mm) |
| Cant | Rifle cant angle at the shot instant, and its spread during the last 1.0 s (degrees) |

Per session: number of shots, total and average score, group centre and group size (largest centre-to-centre distance), averages and trends of the per-shot metrics.

The time windows are configurable.

## 10. User interface

The UI is in English.

| View | Content |
|---|---|
| Live | Target graphic with the moving aim point and the trace so far; state indicator (idle / aiming / shot); audio level meter; small camera preview for setup. Update latency 100 ms or less. |
| Shot | Target graphic with the complete trace, colour-coded by time (approach, last 1.0 s, last 0.2 s, after the shot), the shot marker drawn at pellet size, score, metrics, x / y over time chart, timeline scrubber to replay the trace. Drag the shot marker to calibrate. Delete shot. Debug video playback if recorded. |
| Session | Shot list with scores and key metrics, group plot of all shots, session totals, notes. |
| Compare | Overlay the traces of selected shots (aligned at the shot instant), within one session or across sessions. |
| History | List of past sessions per shooter with date, shot count and average; reopen any session. |
| Shooters | Create, rename, delete shooter profiles. Each has its own rifle profiles, calibration, sound profile and history. |
| Settings | Camera and microphone selection, resolution and frame rate, exposure lock, target profile, pre-roll and follow-through length, detection mode and sensitivity, shot-sound teaching, audio-video offset, debug video on / off. |

A session has: shooter, rifle profile, target profile, mode (live fire / dry fire), start time, free-text notes.

## 11. Data storage

Stored locally on the PC in a local database plus a data folder.

Per shot:
- Identification: session, sequence number, timestamps.
- Raw trace: list of samples `(time relative to shot, x px, y px, mark diameter px, confidence)`.
- Scale, roll and calibration offset in effect.
- Shot instant, detection mode and confidence, flag for manually added or corrected.
- Derived values: calibrated shot position, score, metrics (recalculable from raw data).
- Audio clip from pre-roll to end of follow-through.
- Debug video of the same span, only if debug recording is on. Old debug videos are removed automatically above a configurable disk limit.

## 12. Electronic target integration (nice to have, later)

Possibilities, in order of effort:

1. **Manual** (covered by section 8): the shooter reads the shot from the target monitor and drags the marker.
2. **Result import**: read the shot list from the target system's result file or network interface after the session and match shots by sequence and time; use the real coordinates to set the calibration offset automatically and to show real score next to estimated score.
3. **Live feed**: receive each shot from the target system as it happens; automatic continuous calibration, and the real hit shown in the shot view immediately.
4. **Shot trigger**: use the target system's shot event as an additional confirmation of the own shot in strict detection mode.

Meyton is the first system to support. **[open]** Which interfaces Meyton offers for reading shot data (to be investigated when this work starts).

## 13. Non-functional requirements

- Live aim point latency: 100 ms or less from camera to screen.
- Shot result on screen: within 1 s after follow-through ends.
- Start-up to ready: under 10 s, with the last used camera, shooter and settings restored.
- Robust to camera unplug / replug without restarting the program.
- No network connection required. UI reachable from other devices on the local network only when enabled in the settings.

## 14. Acceptance tests

| Test | Pass condition |
|---|---|
| Rifle clamped, 60 s tracking | Aim position noise 0.1 mm RMS or better |
| Target moved by a known amount (e.g. 10 mm) | Measured displacement within 2 % |
| 20 live shots alone, simple mode | 20 detected, 0 false |
| 20 dry-fire shots alone, simple mode | 20 detected, 0 false |
| 20 own shots with a neighbour firing 20 shots, strict mode | 20 detected, 0 false |
| Calibrated live shots on paper | Estimated position within 1.0 mm of the real hit for 90 % of shots |
| Pre-roll | Each recording starts the configured time before target detection |
| Follow-through | Each recording ends 2 s after the shot |

## 15. Suggested build order

1. Capture and tracking: camera setup, target detection, live aim point in the browser.
2. Shot detection (simple mode), shot lifecycle, pre-roll and follow-through, shot view with trace.
3. Storage, sessions, history, drag-to-calibrate, score.
4. Metrics, compare / overlay, shooter and rifle profiles.
5. Strict detection mode with sound teaching and jolt confirmation; debug video.
6. Electronic target integration.

## 16. Open questions

- Exact camera and lens model, to confirm field of view and frame rate.
- Which interfaces the Meyton system offers for reading shot data (later, with section 12).
- Whether the target surroundings always show enough straight edges for automatic cant estimation, on paper and on Meyton targets (to be checked with real camera images).
