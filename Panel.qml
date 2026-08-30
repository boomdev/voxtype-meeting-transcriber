import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

Panel {
  id: root
  moduleName: "io.github.boomdev.voxtype-meeting-transcriber"
  ipcTarget: moduleName

  property string page: "meeting"
  property double nowMs: Date.now()
  property string meetingTitle: ""

  property string draftSource: "both"
  property bool draftRetainAudio: false
  property string draftMicDevice: "default"
  property string draftLoopbackDevice: "default"
  property string draftExportDirectory: "~/Documents/Meetings"

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property color accent: Color.accent
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property color surface: Color.popups.background
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property bool hasError: backend.lastError !== ""
  readonly property bool meetingTransition: backend.busy
    && (backend.pendingAction === "start" || backend.pendingAction === "stop")
  readonly property string statusLabel: !backend.available ? "Unavailable"
    : backend.recording ? "Recording"
    : backend.paused ? "Paused" : "Ready"
  readonly property color statusColor: hasError || !backend.available ? urgent
    : backend.recording ? urgent
    : backend.paused ? accent : foreground
  readonly property string statusGlyph: meetingTransition ? "󰔟"
    : hasError || !backend.available ? "󰅚"
    : backend.recording ? "󰑊"
    : backend.paused ? "󰏤" : "󰔊"
  readonly property int elapsedSecs: {
    if (!backend.activeMeeting || !backend.activeMeeting.startedAt) return 0
    var started = new Date(String(backend.activeMeeting.startedAt)).getTime()
    return isFinite(started) ? Math.max(0, Math.floor((nowMs - started) / 1000)) : 0
  }

  function formatClock(seconds) {
    var value = Math.max(0, Number(seconds || 0))
    var hours = Math.floor(value / 3600)
    var minutes = Math.floor((value % 3600) / 60)
    var secs = Math.floor(value % 60)
    return hours > 0
      ? hours + ":" + String(minutes).padStart(2, "0") + ":" + String(secs).padStart(2, "0")
      : String(minutes).padStart(2, "0") + ":" + String(secs).padStart(2, "0")
  }

  function formatDuration(seconds) {
    var value = Math.max(0, Number(seconds || 0))
    if (value < 60) return Math.floor(value) + " sec"
    if (value < 3600) return Math.floor(value / 60) + " min"
    return Math.floor(value / 3600) + " h " + Math.floor((value % 3600) / 60) + " min"
  }

  function formatDate(value) {
    var date = new Date(String(value || ""))
    return isNaN(date.getTime()) ? "" : Qt.formatDateTime(date, "d MMM · HH:mm")
  }

  function meetingIsTranscribing(item) {
    return String((item && item.status) || "") === "transcribing"
  }

  function recentMeetingMeta(item) {
    var line = root.formatDate(item && item.startedAt) + "  ·  " + root.formatDuration(item && item.durationSecs)
    if (root.meetingIsTranscribing(item)) line += "  ·  Transcribing"
    return line + "  ·  " + Number((item && item.chunkCount) || 0) + " chunks"
  }

  function activeTitle() {
    if (backend.activeMeeting && backend.activeMeeting.title) return backend.activeMeeting.title
    return backend.active ? "Active meeting" : "Meeting capture"
  }

  function syncDrafts() {
    var o = backend.options || {}
    draftSource = String(o.source || "both")
    draftRetainAudio = o.retainAudio === true
    draftMicDevice = String(o.micDevice || "default")
    draftLoopbackDevice = String(o.loopbackDevice || "default")
    draftExportDirectory = String(root.setting("exportDirectory", "~/Documents/Meetings"))
  }

  function showSettings() {
    syncDrafts()
    page = "settings"
    if (panelFlick) panelFlick.contentY = 0
  }

  function showMeeting() {
    page = "meeting"
    if (panelFlick) panelFlick.contentY = 0
  }

  function captureOptionsDirty() {
    var o = backend.options || {}
    return String(o.source || "both") !== root.draftSource
      || (o.retainAudio === true) !== root.draftRetainAudio
      || String(o.micDevice || "default") !== root.draftMicDevice
      || String(o.loopbackDevice || "default") !== root.draftLoopbackDevice
  }

  function saveMeetingOptions() {
    if (backend.active || !root.captureOptionsDirty()) return
    backend.saveOptions({
      source: draftSource,
      retainAudio: draftRetainAudio,
      micDevice: draftMicDevice,
      loopbackDevice: draftLoopbackDevice
    })
  }

  function currentSource() {
    var o = backend.options || {}
    return String(o.source || root.draftSource || "both")
  }

  function captureIncludesMic() {
    var source = root.currentSource()
    return source === "both" || source === "mic"
  }

  function captureIncludesSystem() {
    var source = root.currentSource()
    return source === "both" || source === "system"
  }

  function toggleCaptureSource(which) {
    if (backend.active || backend.busy) return
    root.syncDrafts()
    var mic = root.draftSource === "both" || root.draftSource === "mic"
    var sys = root.draftSource === "both" || root.draftSource === "system"
    if (which === "mic") mic = !mic
    else if (which === "system") sys = !sys
    if (!mic && !sys) return
    root.draftSource = mic && sys ? "both" : (mic ? "mic" : "system")
    root.saveMeetingOptions()
  }

  function saveExportDirectory() {
    var value = String(root.draftExportDirectory || "").trim()
    if (value === "") return
    root.persistSetting("exportDirectory", value)
    root.draftExportDirectory = value
  }

  function persistSetting(key, value) {
    var entry = { id: root.moduleName }
    for (var existing in root.settings) if (existing !== "id") entry[existing] = root.settings[existing]
    entry[key] = value
    root.settings = entry
    if (root.bar && root.bar.shell && typeof root.bar.shell.updateEntryInline === "function")
      root.bar.shell.updateEntryInline(root.moduleName, entry)
  }

  function asStringList(value, fallback) {
    var defaults = fallback ? fallback.slice() : []
    if (value === undefined || value === null) return defaults
    if (typeof value === "string") {
      var text = value.trim()
      if (text === "") return defaults
      if (text.charAt(0) === "[") {
        try { value = JSON.parse(text) } catch (e) {
          return text.split(",").map(function(item) { return item.trim().toLowerCase() }).filter(function(item) { return item !== "" })
        }
      } else {
        return text.split(",").map(function(item) { return item.trim().toLowerCase() }).filter(function(item) { return item !== "" })
      }
    }
    if (typeof value.length === "number") {
      var out = []
      for (var i = 0; i < value.length; i++) {
        var code = String(value[i] || "").trim().toLowerCase()
        if (code !== "") out.push(code)
      }
      return out
    }
    return defaults
  }

  function languageValue(item) {
    return (item && typeof item === "object") ? String(item.value || "").toLowerCase() : String(item || "").toLowerCase()
  }

  function languageLabel(item) {
    if (item && typeof item === "object" && item.label !== undefined) return String(item.label)
    return String(item || "").toUpperCase()
  }

  function availableLanguages() {
    var raw = backend.options && backend.options.availableLanguages
    if (!raw || typeof raw.length !== "number") return []
    var out = []
    for (var i = 0; i < raw.length; i++) out.push(raw[i])
    return out
  }

  function enabledLanguages() {
    return root.asStringList(root.setting("enabledLanguages", ["auto", "en"]), ["auto", "en"])
  }

  function popupLanguages() {
    var enabled = root.enabledLanguages()
    var allowed = {}
    for (var i = 0; i < enabled.length; i++) allowed[enabled[i]] = true
    var current = root.voxtypeLanguage()
    if (current !== "") allowed[current] = true
    var out = []
    var seen = {}
    var available = root.availableLanguages()
    for (var j = 0; j < available.length; j++) {
      var value = root.languageValue(available[j])
      if (value && allowed[value] && !seen[value]) {
        seen[value] = true
        out.push(available[j])
      }
    }
    if (current !== "" && !seen[current] && !root.modelIsEnglishOnly())
      out.push({ value: current, label: current.toUpperCase() })
    return out
  }

  function voxtypeLanguage() {
    if (root.modelIsEnglishOnly()) return "en"
    var code = String((backend.options && backend.options.language) || "").trim().toLowerCase()
    return code !== "" ? code : "en"
  }

  function effectiveLanguage() {
    if (root.modelIsEnglishOnly()) return "en"
    return root.voxtypeLanguage()
  }

  function startLanguage() {
    return root.effectiveLanguage()
  }

  function modelIsEnglishOnly() {
    var model = String((backend.options && backend.options.model) || "").toLowerCase()
    var name = model.split("/").pop()
    return name.slice(-3) === ".en" || name.indexOf(".en.") !== -1
  }

  readonly property var meetingLanguageChips: {
    var _rev = backend.revision
    var _settings = root.settings
    return root.popupLanguages()
  }

  readonly property bool recentTranscribing: {
    var _rev = backend.revision
    var meetings = backend.recentMeetings || []
    for (var i = 0; i < meetings.length; i++) {
      if (root.meetingIsTranscribing(meetings[i])) return true
    }
    return false
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onOpenedChanged: if (opened) {
    nowMs = Date.now()
    backend.refresh()
    Qt.callLater(function() { keyCatcher.forceActiveFocus() })
  }

  Backend {
    id: backend
    exportDirectory: String(root.setting("exportDirectory", "~/Documents/Meetings"))
    idleRefreshSec: Number(root.setting("refreshIntervalSec", 15))
    fastRefresh: root.recentTranscribing
    notificationsEnabled: root.setting("notificationsEnabled", false) === true

    onCommandFinished: function(kind, ok, message, result) {
      if (kind === "start" && ok) root.meetingTitle = ""
    }
  }

  Timer {
    interval: 1000
    repeat: true
    running: root.opened && backend.active
    onTriggered: root.nowMs = Date.now()
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    active: backend.recording || backend.paused || root.hasError
    activeColor: root.statusColor
    tooltipText: ""
    iconComponent: Component {
      Item {
        OpticalGlyph {
          anchors.centerIn: parent
          width: parent.width
          height: parent.height
          text: root.statusGlyph
          color: root.statusColor
          fontFamily: root.fontFamily
          fontSize: Style.bar.iconFont
        }

        Rectangle {
          visible: backend.recording
          width: Style.space(4)
          height: width
          radius: width / 2
          color: root.urgent
          anchors.right: parent.right
          anchors.bottom: parent.bottom

          SequentialAnimation on opacity {
            running: backend.recording
            loops: Animation.Infinite
            NumberAnimation { to: 0.25; duration: 650; easing.type: Easing.InOutSine }
            NumberAnimation { to: 1.0; duration: 650; easing.type: Easing.InOutSine }
          }
        }
      }
    }
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton) backend.refresh()
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(410))
    contentHeight: panel.fittedContentHeight(
      headerRow.implicitHeight + Style.space(12) + pageLoader.implicitHeight, Style.space(650))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(text) {
        if (text === "r" || text === "R") backend.refresh()
        else if ((text === "s" || text === "S") && root.page === "meeting" && backend.available && !backend.busy) {
          if (backend.active) backend.runAction("stop", "")
          else backend.runAction("start", root.meetingTitle, root.startLanguage())
        }
      }

      ColumnLayout {
        anchors.fill: parent
        spacing: Style.space(12)

        RowLayout {
          id: headerRow
          Layout.fillWidth: true
          spacing: Style.space(8)

          Text {
            text: root.page === "meeting" ? "MEETING TRANSCRIBER" : "MEETING TRANSCRIBER OPTIONS"
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            font.bold: true
            font.letterSpacing: 1.4
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignVCenter
          }

          Text {
            visible: backend.actionStatus !== ""
            text: backend.actionStatus
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            elide: Text.ElideRight
            Layout.maximumWidth: Style.space(210)
            Layout.alignment: Qt.AlignVCenter
          }

          IconButton {
            iconText: root.page === "meeting" ? "󰒓" : "󰁍"
            tooltipText: root.page === "meeting" ? "Meeting options" : "Back to meeting"
            foreground: root.foreground
            fontFamily: root.fontFamily
            focusable: true
            onClicked: root.page === "meeting" ? root.showSettings() : root.showMeeting()
          }
        }

        Flickable {
          id: panelFlick
          Layout.fillWidth: true
          Layout.fillHeight: true
          contentWidth: width
          contentHeight: pageLoader.implicitHeight
          clip: true
          boundsBehavior: Flickable.StopAtBounds
          flickableDirection: Flickable.VerticalFlick
          interactive: contentHeight > height
          ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

          Loader {
            id: pageLoader
            width: panelFlick.width
            sourceComponent: root.page === "meeting" ? meetingPage : settingsPage
          }
        }
      }
    }
  }

  Component {
    id: meetingPage

    Column {
      width: parent ? parent.width : 0
      spacing: Style.space(12)

      BorderSurface {
        width: parent.width
        implicitHeight: heroContent.implicitHeight + Style.space(28)
        radius: Math.max(Style.cornerRadius, Style.space(8))
        color: backend.recording
          ? Qt.rgba(root.urgent.r, root.urgent.g, root.urgent.b, 0.09)
          : Style.normalFillFor(root.foreground, root.accent)
        borderSpec: Border.controlSpec(backend.recording ? "selected" : "normal", root.statusColor, root.accent)

        Behavior on color { ColorAnimation { duration: 220 } }

        Column {
          id: heroContent
          anchors.left: parent.left
          anchors.right: parent.right
          anchors.verticalCenter: parent.verticalCenter
          anchors.leftMargin: Style.space(16)
          anchors.rightMargin: Style.space(16)
          spacing: Style.space(12)

          PanelHero {
            width: parent.width
            title: root.activeTitle()
            meta: root.statusLabel
            detail: backend.active ? root.formatClock(root.elapsedSecs) : ""
            foreground: root.foreground
            fontFamily: root.fontFamily
            iconComponent: Component {
              Item {
                implicitWidth: Style.font.display
                implicitHeight: Style.font.display

                Rectangle {
                  anchors.centerIn: parent
                  width: parent.width
                  height: width
                  radius: width / 2
                  color: Qt.rgba(root.statusColor.r, root.statusColor.g, root.statusColor.b, 0.12)
                  scale: backend.recording ? 1.0 : 0.88
                  Behavior on scale { NumberAnimation { duration: 220; easing.type: Easing.OutCubic } }
                }

                Item {
                  id: statusGlyphSpin
                  anchors.centerIn: parent
                  width: Style.font.display
                  height: Style.font.display
                  rotation: 0
                  transformOrigin: Item.Center

                  OpticalGlyph {
                    anchors.fill: parent
                    text: root.statusGlyph
                    color: root.statusColor
                    fontFamily: root.fontFamily
                    fontSize: Style.font.heading
                  }

                  NumberAnimation on rotation {
                    running: root.meetingTransition
                    from: 0
                    to: 360
                    duration: 900
                    loops: Animation.Infinite
                    onRunningChanged: if (!running) statusGlyphSpin.rotation = 0
                  }

                  onRotationChanged: if (!root.meetingTransition && rotation !== 0) rotation = 0
                }
              }
            }
          }

          RowLayout {
            visible: backend.active
            width: parent.width
            spacing: Style.space(8)

            Text {
              text: "󰄶  " + Number(backend.activeMeeting ? backend.activeMeeting.chunkCount || 0 : 0) + " chunks"
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              Layout.fillWidth: true
            }

            Text {
              text: root.languageLabel(root.effectiveLanguage())
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
            }

            Text {
              text: backend.options && backend.options.source === "both" ? "󰍬 + 󰕾  mic & system"
                : backend.options && backend.options.source === "system" ? "󰕾  system" : "󰍬  microphone"
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
            }
          }
        }
      }

      Text {
        visible: root.hasError
        width: parent.width
        text: backend.lastError
        color: root.urgent
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WordWrap
      }

      RowLayout {
        width: parent.width
        spacing: Style.space(8)
        visible: !backend.active

        Flow {
          Layout.fillWidth: true
          Layout.alignment: Qt.AlignVCenter
          spacing: Style.space(8)

          Repeater {
            model: !root.modelIsEnglishOnly() ? root.meetingLanguageChips : []

            Button {
              required property var modelData
              text: root.languageLabel(modelData)
              selected: {
                var _revision = backend.revision
                return root.effectiveLanguage() === root.languageValue(modelData)
              }
              bordered: true
              foreground: root.foreground
              fontFamily: root.fontFamily
              enabled: !backend.busy && !backend.languageBusy
              onClicked: backend.setVoxtypeLanguage(root.languageValue(modelData))
            }
          }
        }

        Button {
          iconText: "󰍬"
          tooltipText: "Microphone"
          foreground: root.foreground
          fontFamily: root.fontFamily
          iconSize: Style.font.body
          iconRotation: 0
          bordered: true
          selected: {
            var _revision = backend.revision
            return root.captureIncludesMic()
          }
          enabled: !backend.busy
          Layout.alignment: Qt.AlignVCenter
          onClicked: root.toggleCaptureSource("mic")
        }

        Button {
          iconText: "󰕾"
          tooltipText: "System audio"
          foreground: root.foreground
          fontFamily: root.fontFamily
          iconSize: Style.font.body
          iconRotation: 0
          bordered: true
          selected: {
            var _revision = backend.revision
            return root.captureIncludesSystem()
          }
          enabled: !backend.busy
          Layout.alignment: Qt.AlignVCenter
          onClicked: root.toggleCaptureSource("system")
        }
      }

      Column {
        visible: !backend.active
        width: parent.width
        spacing: Style.space(8)

        TextField {
          width: parent.width
          text: root.meetingTitle
          placeholderText: "Optional meeting title"
          foreground: root.foreground
          enabled: backend.available && !backend.busy
          onTextChanged: root.meetingTitle = text
          onAccepted: if (backend.available && !backend.busy) backend.runAction("start", text, root.startLanguage())
        }

        Button {
          width: parent.width
          text: "Start meeting"
          iconText: backend.busy ? "󰑮" : "󰑊"
          iconRotation: 0
          selected: true
          foreground: root.foreground
          fontFamily: root.fontFamily
          enabled: backend.available && !backend.busy
          onClicked: backend.runAction("start", root.meetingTitle, root.startLanguage())
        }
      }

      RowLayout {
        visible: backend.active
        width: parent.width
        spacing: Style.space(8)

        Button {
          Layout.fillWidth: true
          text: backend.paused ? "Resume" : "Pause"
          iconText: backend.paused ? "󰐊" : "󰏤"
          iconRotation: 0
          foreground: root.foreground
          fontFamily: root.fontFamily
          enabled: !backend.busy
          onClicked: backend.runAction(backend.paused ? "resume" : "pause", "")
        }

        Button {
          Layout.fillWidth: true
          text: "Stop & finish"
          iconText: backend.pendingAction === "stop" ? "󰑮" : "󰓛"
          iconRotation: 0
          foreground: root.urgent
          fontFamily: root.fontFamily
          enabled: !backend.busy
          onClicked: backend.runAction("stop", "")
        }
      }

      PanelSeparator { foreground: root.foreground }

      RowLayout {
        width: parent.width

        PanelSectionHeader {
          text: "RECENT MEETINGS"
          foreground: root.foreground
          fontFamily: root.fontFamily
          Layout.fillWidth: true
        }

        IconButton {
          iconText: "󰉋"
          tooltipText: "Open transcripts folder"
          foreground: root.foreground
          fontFamily: root.fontFamily
          focusable: true
          onClicked: backend.openExportFolder()
        }

        IconButton {
          iconText: "󰑐"
          tooltipText: "Refresh"
          foreground: root.foreground
          fontFamily: root.fontFamily
          focusable: true
          enabled: !backend.busy
          onClicked: backend.refresh()
        }
      }

      Text {
        visible: backend.recentMeetings.length === 0
        width: parent.width
        text: backend.available ? "Your completed meetings will appear here." : "Meeting history is unavailable."
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
        horizontalAlignment: Text.AlignHCenter
        wrapMode: Text.WordWrap
        topPadding: Style.space(10)
        bottomPadding: Style.space(10)
      }

      Column {
        visible: backend.recentMeetings.length > 0
        width: parent.width
        spacing: Style.space(6)

        Repeater {
          model: backend.recentMeetings

          BorderSurface {
            required property var modelData
            width: parent.width
            implicitHeight: recentRow.implicitHeight + Style.space(18)
            radius: Style.cornerRadius
            color: Style.normalFillFor(root.foreground, root.accent)
            borderSpec: Border.controlSpec("normal", root.foreground, root.accent)

            RowLayout {
              id: recentRow
              anchors.left: parent.left
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              anchors.leftMargin: Style.space(10)
              anchors.rightMargin: Style.space(8)
              spacing: Style.space(8)

              ColumnLayout {
                Layout.fillWidth: true
                spacing: Style.space(2)

                Text {
                  text: String(modelData.title || "Meeting")
                  color: root.foreground
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                  font.bold: true
                  elide: Text.ElideRight
                  Layout.fillWidth: true
                }

                Text {
                  text: root.recentMeetingMeta(modelData)
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.caption
                  elide: Text.ElideRight
                  Layout.fillWidth: true
                }
              }

              IconButton {
                iconText: String(modelData.exportedPath || "") !== "" ? "󰏌" : "󰈙"
                tooltipText: root.meetingIsTranscribing(modelData)
                  ? "Wait until transcription finishes"
                  : (String(modelData.exportedPath || "") !== "" ? "Open" : "Export and Open")
                foreground: root.foreground
                fontFamily: root.fontFamily
                focusable: true
                enabled: !backend.busy && !root.meetingIsTranscribing(modelData)
                onClicked: {
                  if (String(modelData.exportedPath || "") !== "") backend.openMeeting(modelData.id)
                  else backend.exportMeeting(modelData.id, true)
                }
              }
            }
          }
        }
      }

    }
  }

  Component {
    id: settingsPage

    Column {
      width: parent ? parent.width : 0
      spacing: Style.space(12)

      BorderSurface {
        visible: backend.active
        width: parent.width
        implicitHeight: lockedText.implicitHeight + Style.space(20)
        radius: Style.cornerRadius
        color: Qt.rgba(root.urgent.r, root.urgent.g, root.urgent.b, 0.08)
        borderSpec: Border.controlSpec("normal", root.urgent, root.accent)

        Text {
          id: lockedText
          anchors.left: parent.left
          anchors.right: parent.right
          anchors.verticalCenter: parent.verticalCenter
          anchors.margins: Style.space(10)
          text: "󰌾  Finish the active meeting before changing capture options."
          color: root.urgent
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.WordWrap
        }
      }

      PanelSectionHeader { text: "CAPTURE"; foreground: root.foreground; fontFamily: root.fontFamily }

      Toggle {
        width: parent.width
        label: "Retain meeting audio"
        description: "Keep original audio alongside the transcript."
        checked: root.draftRetainAudio
        foreground: root.foreground
        enabled: !backend.active && !backend.busy
        onClicked: {
          root.draftRetainAudio = !root.draftRetainAudio
          root.saveMeetingOptions()
        }
      }

      Dropdown {
        width: parent.width
        label: "Microphone device"
        value: root.draftMicDevice
        options: backend.microphoneDevices
        foreground: root.foreground
        enabled: !backend.active && !backend.busy
        onChanged: function(value) {
          root.draftMicDevice = value
          root.saveMeetingOptions()
        }
      }

      Dropdown {
        width: parent.width
        label: "System audio device"
        value: root.draftLoopbackDevice
        options: backend.outputDevices
        foreground: root.foreground
        enabled: !backend.active && !backend.busy
        onChanged: function(value) {
          root.draftLoopbackDevice = value
          root.saveMeetingOptions()
        }
      }

      PanelSeparator { foreground: root.foreground }
      PanelSectionHeader { text: "TRANSCRIPTION ENGINE"; foreground: root.foreground; fontFamily: root.fontFamily }

      Text {
        width: parent.width
        text: String(backend.options.engine || "Voxtype") + " · " + String(backend.options.model || "configured model")
        color: root.foreground
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
        font.bold: true
        wrapMode: Text.WordWrap
      }

      Text {
        visible: root.modelIsEnglishOnly()
        width: parent.width
        text: "This model is English-only, so meetings are limited to English. Choose a multilingual model in Voxtype settings to pick other languages here."
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
        wrapMode: Text.WordWrap
      }

      MultiSelect {
        visible: !root.modelIsEnglishOnly()
        width: parent.width
        label: "Languages in the meeting popup"
        values: {
          var _settings = root.settings
          var _revision = backend.revision
          return root.enabledLanguages()
        }
        options: {
          var _revision = backend.revision
          return root.availableLanguages()
        }
        noSelectionText: "None selected"
        foreground: root.foreground
        enabled: !backend.active && !backend.busy
        onChanged: function(values) { root.persistSetting("enabledLanguages", values) }
      }

      PanelSeparator { foreground: root.foreground }
      PanelSectionHeader { text: "TRAY"; foreground: root.foreground; fontFamily: root.fontFamily }

      Toggle {
        width: parent.width
        label: "Desktop notifications"
        description: "Meeting lifecycle, exports, and failures."
        checked: root.setting("notificationsEnabled", false) === true
        foreground: root.foreground
        onClicked: root.persistSetting("notificationsEnabled", !(root.setting("notificationsEnabled", false) === true))
      }

      RowLayout {
        width: parent.width
        spacing: Style.space(8)

        TextField {
          id: exportPathField
          Layout.fillWidth: true
          Layout.alignment: Qt.AlignVCenter
          text: root.draftExportDirectory
          placeholderText: "Transcript export folder"
          foreground: root.foreground
          onTextChanged: root.draftExportDirectory = text
        }

        IconButton {
          iconText: "󰆓"
          tooltipText: "Save export folder"
          foreground: root.foreground
          fontFamily: root.fontFamily
          focusable: true
          Layout.alignment: Qt.AlignVCenter
          enabled: String(root.draftExportDirectory || "").trim() !== ""
            && String(root.draftExportDirectory || "").trim() !== String(root.setting("exportDirectory", "~/Documents/Meetings"))
          onClicked: root.saveExportDirectory()
        }
      }

      Text {
        visible: root.hasError
        width: parent.width
        text: backend.lastError
        color: root.urgent
        font.family: root.fontFamily
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WordWrap
      }
    }
  }
}
