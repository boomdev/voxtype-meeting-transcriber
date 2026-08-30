import QtQuick
import Quickshell
import Quickshell.Io

Item {
  id: root

  property string helperPath: pluginFile("bin/voxtype-meeting-tray")
  readonly property string installScriptPath: pluginFile("scripts/install-user.sh")
  property string exportDirectory: "~/Documents/Meetings"
  property int idleRefreshSec: 15
  property bool fastRefresh: false
  property bool notificationsEnabled: false

  property bool available: false
  property bool installed: false
  property string meetingState: "idle"
  property var activeMeeting: null
  property var recentMeetings: []
  property var options: ({})
  property var microphoneDevices: []
  property var outputDevices: []
  property string configPath: ""
  property string lastError: ""
  property string actionStatus: ""
  property bool busy: false
  property bool languageBusy: false
  property bool settingsBusy: false
  property var pendingSettings: null
  property string pendingAction: ""
  property double revision: 0

  readonly property bool active: meetingState === "recording" || meetingState === "paused"
  readonly property bool recording: meetingState === "recording"
  readonly property bool paused: meetingState === "paused"

  signal commandFinished(string kind, bool ok, string message, var result)

  function pluginFile(relative) {
    var url = Qt.resolvedUrl(relative).toString()
    if (url.indexOf("file://") === 0) {
      url = url.slice(7)
      if (url.indexOf("localhost") === 0) url = url.slice(9)
    }
    try { return decodeURIComponent(url) } catch (e) { return url }
  }

  function installCaptureService() {
    if (installProcess.running || !installScriptPath) return
    installProcess.command = ["omarchy-launch-floating-terminal-with-presentation", installScriptPath]
    installProcess.running = true
  }

  function startCaptureService() {
    if (startProcess.running) return
    startProcess.command = ["systemctl", "--user", "start", "voxtype-meeting-service.service"]
    startProcess.running = true
  }

  function parseJson(text, fallbackError) {
    try {
      var parsed = JSON.parse(String(text || "").trim())
      if (parsed && typeof parsed === "object") return parsed
    } catch (e) {}
    return { ok: false, error: fallbackError || "The Voxtype helper returned invalid data." }
  }

  function applySnapshot(data) {
    installed = data.installed === true
    available = data.available === true
    meetingState = String(data.state || "idle")
    activeMeeting = data.active || null
    recentMeetings = Array.isArray(data.recent) ? data.recent : []
    options = data.options || ({})
    microphoneDevices = Array.isArray(options.micDevices) ? options.micDevices : []
    outputDevices = Array.isArray(options.outputDevices) ? options.outputDevices : []
    configPath = String(data.configPath || "")
    if (!busy) lastError = String(data.error || "")
    revision++
  }

  function refresh() {
    if (snapshotProcess.running) return
    snapshotProcess.running = true
  }

  function runAction(name, title, language) {
    if (busy) return
    var command = [helperPath, "action", name]
    if (name === "start" && String(title || "").trim() !== "")
      command.push("--title", String(title).trim())
    if (name === "start" && String(language || "").trim() !== "")
      command.push("--language", String(language).trim())
    launch(command, name)
  }

  function exportMeeting(id, openAfter) {
    if (busy) return
    var command = [helperPath, "export", String(id || "latest"), "--directory", exportDirectory]
    if (openAfter) command.push("--open")
    launch(command, openAfter ? "open-export" : "export")
  }

  function openMeeting(id) {
    if (busy) return
    launch([helperPath, "open", String(id || "latest")], "open-export")
  }

  function openExportFolder() {
    if (folderProcess.running) return
    lastError = ""
    folderProcess.command = [helperPath, "open-folder", "--directory", exportDirectory]
    folderProcess.running = true
  }

  function saveOptions(values) {
    if (active) return
    pendingSettings = values
    var next = {}
    var key
    for (key in options) next[key] = options[key]
    next.source = values.source
    next.retainAudio = values.retainAudio === true
    next.micDevice = values.micDevice
    next.loopbackDevice = values.loopbackDevice
    options = next
    revision++
    flushSettings()
  }

  function flushSettings() {
    if (settingsProcess.running || !pendingSettings) return
    var values = pendingSettings
    pendingSettings = null
    settingsBusy = true
    lastError = ""
    settingsProcess.command = [helperPath, "settings", JSON.stringify(values)]
    settingsProcess.running = true
  }

  function setVoxtypeLanguage(code) {
    var language = String(code || "").trim().toLowerCase()
    if (language === "" || languageProcess.running) return
    var next = {}
    var key
    for (key in options) next[key] = options[key]
    next.language = language
    options = next
    revision++
    languageBusy = true
    lastError = ""
    languageProcess.command = [helperPath, "language", language]
    languageProcess.running = true
  }

  function launch(command, kind) {
    busy = true
    pendingAction = kind
    actionStatus = kind === "start" ? "Please wait — starting meeting…"
      : kind === "stop" ? "Please wait — finishing meeting…"
      : kind === "pause" ? "Please wait — pausing meeting…"
      : kind === "resume" ? "Please wait — resuming meeting…"
      : kind === "export" || kind === "open-export" ? "Preparing transcript…"
      : kind === "settings" ? "Saving meeting options…"
      : kind.charAt(0).toUpperCase() + kind.slice(1) + "ing…"
    lastError = ""
    actionProcess.kind = kind
    actionProcess.command = command
    actionProcess.running = true
  }

  function notify(kind, ok, message, result) {
    if (!notificationsEnabled) return
    var headline = ok ? "Voxtype Meeting" : "Voxtype Meeting Error"
    var body = message
    var glyph = ok ? "󰍬" : "󰅚"
    notificationProcess.command = ["omarchy-notification-send", "--app-name", "Voxtype Meeting Transcriber",
                                   "-g", glyph, "-u", ok ? "low" : "critical", headline, body]
    if (!notificationProcess.running) notificationProcess.running = true
  }

  Process {
    id: snapshotProcess
    command: [root.helperPath, "snapshot"]
    stdout: StdioCollector { id: snapshotOutput; waitForEnd: true }
    stderr: StdioCollector { id: snapshotError; waitForEnd: true }
    onExited: function(code) {
      var data = root.parseJson(snapshotOutput.text, snapshotError.text || "Could not inspect Voxtype.")
      if (data.ok === true) root.applySnapshot(data)
      else root.lastError = String(data.error || "Could not inspect Voxtype.")
    }
  }

  Process {
    id: actionProcess
    property string kind: ""
    stdout: StdioCollector { id: actionOutput; waitForEnd: true }
    stderr: StdioCollector { id: actionError; waitForEnd: true }
    onExited: function(code) {
      var data = root.parseJson(actionOutput.text, actionError.text || "Voxtype command failed.")
      var ok = data.ok === true
      var message = ok ? String(data.message || "Done") : String(data.error || "Voxtype command failed.")
      root.busy = false
      root.pendingAction = ""
      root.actionStatus = ""
      root.lastError = ok ? "" : message
      root.commandFinished(kind, ok, message, data)
      root.notify(kind, ok, message, data)
      Qt.callLater(root.refresh)
    }
  }

  Process {
    id: settingsProcess
    stdout: StdioCollector { id: settingsOutput; waitForEnd: true }
    stderr: StdioCollector { id: settingsError; waitForEnd: true }
    onExited: function(code) {
      var data = root.parseJson(settingsOutput.text, settingsError.text || "Could not save meeting options.")
      root.settingsBusy = false
      if (data.ok === true) {
        if (!root.busy) root.lastError = ""
      } else {
        root.lastError = String(data.error || "Could not save meeting options.")
      }
      if (root.pendingSettings) root.flushSettings()
      else Qt.callLater(root.refresh)
    }
  }

  Process {
    id: languageProcess
    stdout: StdioCollector { id: languageOutput; waitForEnd: true }
    stderr: StdioCollector { id: languageError; waitForEnd: true }
    onExited: function(code) {
      var data = root.parseJson(languageOutput.text, languageError.text || "Could not update Voxtype language.")
      root.languageBusy = false
      if (data.ok === true) {
        if (!root.busy) root.lastError = ""
      } else {
        root.lastError = String(data.error || "Could not update Voxtype language.")
      }
      Qt.callLater(root.refresh)
    }
  }

  Process {
    id: folderProcess
    stdout: StdioCollector { id: folderOutput; waitForEnd: true }
    stderr: StdioCollector { id: folderError; waitForEnd: true }
    onExited: function(code) {
      var data = root.parseJson(folderOutput.text, folderError.text || "Could not open the transcripts folder.")
      if (data.ok === true) {
        if (!root.busy) root.lastError = ""
      } else {
        root.lastError = String(data.error || "Could not open the transcripts folder.")
      }
    }
  }

  Process { id: notificationProcess }

  Process {
    id: installProcess
    onExited: Qt.callLater(root.refresh)
  }

  Process {
    id: startProcess
    onExited: Qt.callLater(root.refresh)
  }

  Timer {
    interval: (root.active || root.fastRefresh || !root.available) ? 2000 : Math.max(5, root.idleRefreshSec) * 1000
    running: true
    repeat: true
    onTriggered: root.refresh()
  }

  Component.onCompleted: refresh()
}
