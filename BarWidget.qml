import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

BarWidget {
  id: root
  moduleName: "io.github.boomdev.voxtype-meeting-transcriber"

  readonly property var panel: panelLoader.item
  readonly property bool recording: panel ? panel.recording === true : false
  readonly property bool paused: panel ? panel.paused === true : false
  readonly property bool hasError: panel ? panel.hasError === true : false
  readonly property string statusGlyph: panel ? String(panel.statusGlyph || "󰔊") : "󰔊"
  readonly property color statusColor: panel && panel.statusColor ? panel.statusColor
    : (bar ? bar.foreground : Color.foreground)
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  // Shape contract for shell.summon/hide/toggle routing: Bar.findPanelWidget
  // requires open/close/opened on the bar-widget root.
  readonly property bool opened: panel ? panel.opened === true : false
  readonly property bool popoutSwitchClosing: panel ? panel.popoutSwitchClosing === true : false

  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
    if ("anchorItem" in target) target.anchorItem = button
    if ("hostWidget" in target) target.hostWidget = root
  }

  function open() {
    if (panel && panel.open) panel.open()
  }

  function close() {
    if (panel && panel.close) panel.close()
  }

  function toggle() {
    if (panel && panel.toggle) panel.toggle()
  }

  function closeForPopoutSwitch() {
    if (panel && panel.closeForPopoutSwitch) panel.closeForPopoutSwitch()
  }

  function refresh() {
    if (panel && panel.refresh) panel.refresh()
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onBarChanged: injectPanel()
  onSettingsChanged: injectPanel()

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("Panel.qml")
    visible: false
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }

  IpcHandler {
    target: "io.github.boomdev.voxtype-meeting-transcriber"

    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.toggle() }
    function refresh(): void { root.refresh() }
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    active: root.recording || root.paused || root.hasError
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
          visible: root.recording
          width: Style.space(4)
          height: width
          radius: width / 2
          color: root.urgent
          anchors.right: parent.right
          anchors.bottom: parent.bottom

          SequentialAnimation on opacity {
            running: root.recording
            loops: Animation.Infinite
            NumberAnimation { to: 0.25; duration: 650; easing.type: Easing.InOutSine }
            NumberAnimation { to: 1.0; duration: 650; easing.type: Easing.InOutSine }
          }
        }
      }
    }
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton) root.refresh()
      else root.toggle()
    }
  }
}
