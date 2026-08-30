import QtQuick
import qs.Commons
import qs.Ui

// Local copy of PanelActionButton that optically centers Nerd Font glyphs.
BorderSurface {
  id: root

  property string iconText: ""
  property string tooltipText: ""
  property color foreground: Color.foreground
  property color hoverColor: foreground
  property string fontFamily: Style.font.family
  property real fontSize: Style.font.icon
  property real size: Math.max(Style.space(22), fontSize + Style.spacing.sm * 2)

  property bool focusable: false
  property bool hasCursor: false
  property bool bordered: false
  property bool selected: false

  signal clicked()
  signal hovered(bool isHovered)

  activeFocusOnTab: focusable
  Keys.onReturnPressed: if (focusable) root.clicked()
  Keys.onEnterPressed: if (focusable) root.clicked()
  Keys.onSpacePressed: if (focusable) root.clicked()

  implicitWidth: size
  implicitHeight: size
  radius: Style.cornerRadius

  readonly property bool _showFocusRing: focusable && activeFocus
  readonly property bool _hot: (mouse.containsMouse || root.hasCursor) && root.enabled
  readonly property color _selectedColor: Style.selectedStateColor(root.foreground, Color.accent)
  readonly property var _selectedBorderSpec: Border.controlSpec("selected", foreground, Color.accent)
  readonly property var _hoverBorderSpec: Border.controlSpec("hover-cursor", hoverColor, hoverColor)
  readonly property var _normalBorderSpec: Border.controlSpec("normal", foreground, Color.accent)
  readonly property var _borderSpec: _showFocusRing
    ? Border.controlSpec("focus", hoverColor, hoverColor)
    : (_hot
      ? _hoverBorderSpec
      : (selected
        ? (Border.controlHasWidth("selected") ? _selectedBorderSpec : (bordered ? _normalBorderSpec : Border.none()))
        : (bordered ? _normalBorderSpec : Border.none())))

  color: _showFocusRing
    ? Style.focusFillFor(hoverColor, hoverColor)
    : (_hot
      ? Style.hoverFillFor(hoverColor, hoverColor)
      : (selected
        ? Style.selectedFillFor(foreground, Color.accent)
        : "transparent"))
  borderSpec: _borderSpec

  Behavior on color { ColorAnimation { duration: 60 } }

  Item {
    anchors.centerIn: parent
    width: root.fontSize
    height: root.fontSize

    OpticalGlyph {
      anchors.fill: parent
      text: root.iconText
      color: root.enabled
        ? (root._hot ? root.hoverColor : (root.selected ? root._selectedColor : root.foreground))
        : Qt.darker(root.foreground, 2.0)
      fontFamily: root.fontFamily
      fontSize: root.fontSize
    }
  }

  MouseArea {
    id: mouse
    anchors.fill: parent
    hoverEnabled: true
    cursorShape: root.enabled ? Qt.PointingHandCursor : Qt.ArrowCursor
    enabled: root.enabled
    onContainsMouseChanged: root.hovered(containsMouse)
    onClicked: {
      if (root.focusable) root.forceActiveFocus()
      root.clicked()
    }
  }

  PanelToolTip {
    visible: root.tooltipText !== "" && mouse.containsMouse
    text: root.tooltipText
    fontFamily: root.fontFamily
  }
}
