use crate::audio::{AudioBackend, AudioDevice, DeviceRole};
use crate::error::Result;

pub fn print_devices(backend: &dyn AudioBackend) -> Result<()> {
    let inputs = backend.list_input_devices()?;
    let outputs = backend.list_output_devices()?;
    let monitors = backend.list_monitor_sources()?;
    let default_mic = backend.current_microphone()?;
    let default_out = backend.current_output_sink()?;
    let selected_monitor = backend.current_output_monitor()?;

    println!("Microphones:");
    print_group(&inputs, Some(&default_mic.id), "default");
    println!();
    println!("Output sinks:");
    print_group(&outputs, Some(&default_out.id), "default");
    println!();
    println!("Monitor sources:");
    print_group(&monitors, Some(&selected_monitor.id), "selected for SYSTEM");
    println!();
    println!("Default microphone: {}", default_mic.summary());
    println!("Default output: {}", default_out.summary());
    println!("Selected monitor: {}", selected_monitor.summary());
    Ok(())
}

fn print_group(devices: &[AudioDevice], mark_id: Option<&str>, mark_label: &str) {
    if devices.is_empty() {
        println!("  (none)");
        return;
    }
    for device in devices {
        let marker = if mark_id == Some(device.id.as_str()) {
            "*"
        } else {
            " "
        };
        let extra = if mark_id == Some(device.id.as_str()) {
            format!("  ({mark_label})")
        } else {
            String::new()
        };
        let role = match device.role {
            DeviceRole::Microphone => "mic",
            DeviceRole::OutputSink => "sink",
            DeviceRole::MonitorSource => "monitor",
        };
        println!(
            "  {marker} {}  {}  [{role}]{extra}",
            device.id, device.description
        );
    }
}
