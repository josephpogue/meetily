# Common Development Tasks

## Adding a New Audio Device Platform

1. Create platform file: `audio/devices/platform/{platform_name}.rs`
2. Implement device enumeration for the platform
3. Add platform-specific configuration in `audio/devices/configuration.rs`
4. Update `audio/devices/platform/mod.rs` to export new platform functions
5. Test with `cargo check` and platform-specific device tests

## Adding a New Tauri Command

1. Define command in `src/lib.rs`:
   ```rust
   #[tauri::command]
   async fn my_command(arg: String) -> Result<String, String> { /* ... */ }
   ```
2. Register in `tauri::Builder`:
   ```rust
   .invoke_handler(tauri::generate_handler![
       start_recording,
       my_command,  // Add here
   ])
   ```
3. Call from frontend:
   ```typescript
   const result = await invoke<string>('my_command', { arg: 'value' });
   ```

## Modifying Audio Pipeline Behavior

**Location**: `frontend/src-tauri/src/audio/pipeline.rs`

Key components:
- `AudioMixerRingBuffer`: Manages mic + system audio synchronization
- `ProfessionalAudioMixer`: RMS-based ducking and mixing
- `AudioPipelineManager`: Orchestrates VAD, mixing, and distribution

**Testing Audio Changes**:
```bash
# Enable verbose audio logging
RUST_LOG=app_lib::audio=debug ./clean_run.sh

# Monitor audio metrics in real-time
# Check Developer Console in the app (Cmd+Shift+I on macOS)
```
