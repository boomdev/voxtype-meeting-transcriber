import QtQuick
import Quickshell.Io

Process {
  id: root

  property var argv: []
  property int timeoutSec: 15
  property int maxBytes: 262144
  property int killAfterMs: 2000
  property bool timedOut: false
  property bool overflowed: false

  readonly property string stdoutText: stdoutCap.text
  readonly property string stderrText: stderrCap.text

  command: {
    var args = argv
    if (!args || args.length === 0)
      return []
    var graceSec = Math.max(1, Math.round(killAfterMs / 1000))
    var wrapped = ["timeout", "--kill-after=" + graceSec + "s", timeoutSec + "s"]
    for (var i = 0; i < args.length; i++)
      wrapped.push(args[i])
    return wrapped
  }

  stdout: StdioCollector {
    id: stdoutCap
    waitForEnd: false
    onDataChanged: root._checkCap()
  }
  stderr: StdioCollector {
    id: stderrCap
    waitForEnd: false
    onDataChanged: root._checkCap()
  }

  property Timer deadlineTimer: Timer {
    // Let the timeout supervisor enforce the deadline first. This timer is a
    // UI-side watchdog in case the wrapper itself stops making progress.
    interval: Math.max(100, root.timeoutSec * 1000 + 250)
    repeat: false
    onTriggered: root.abort("timeout")
  }

  property Timer killTimer: Timer {
    // GNU timeout owns descendant cleanup. Only kill the wrapper after its
    // --kill-after grace period has had time to terminate and reap the group.
    interval: Math.max(100, root.killAfterMs + 500)
    repeat: false
    onTriggered: {
      if (root.running)
        root.signal(9)
    }
  }

  onStarted: {
    timedOut = false
    overflowed = false
    deadlineTimer.restart()
    killTimer.stop()
  }

  onExited: {
    deadlineTimer.stop()
    killTimer.stop()
  }

  function _byteSize(collector) {
    try {
      var raw = collector.data
      if (raw && raw.byteLength !== undefined)
        return raw.byteLength
    } catch (e) {}
    return String(collector.text || "").length
  }

  function _checkCap() {
    if (root._byteSize(stdoutCap) + root._byteSize(stderrCap) > root.maxBytes)
      root.abort("overflow")
  }

  function abort(kind) {
    if (kind === "timeout")
      root.timedOut = true
    else if (kind === "overflow")
      root.overflowed = true
    deadlineTimer.stop()
    if (!root.running)
      return
    // The direct child is GNU timeout, which forwards TERM to the process
    // group it created and applies --kill-after to resistant descendants.
    // Setting running=false would terminate only the wrapper and could orphan
    // the producer that caused the timeout or overflow.
    root.signal(15)
    killTimer.restart()
  }
}
