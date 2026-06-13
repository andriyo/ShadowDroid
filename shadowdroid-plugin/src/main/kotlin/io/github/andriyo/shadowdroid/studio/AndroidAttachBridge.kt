package io.github.andriyo.shadowdroid.studio

import com.android.ddmlib.AndroidDebugBridge
import com.android.ddmlib.Client
import com.android.ddmlib.ClientData
import com.android.ddmlib.IDevice
import com.android.tools.idea.execution.common.debug.AndroidDebugger
import com.android.tools.idea.execution.common.debug.RunConfigurationWithDebugger
import com.android.tools.idea.execution.common.debug.utils.AndroidConnectDebugger
import com.intellij.execution.RunManager
import com.intellij.execution.RunnerAndConfigurationSettings
import com.intellij.execution.configurations.RunConfiguration
import com.intellij.openapi.actionSystem.ActionManager
import com.intellij.openapi.actionSystem.ActionPlaces
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.actionSystem.CommonDataKeys
import com.intellij.openapi.actionSystem.impl.SimpleDataContext
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.project.Project
import org.jetbrains.android.sdk.AndroidSdkUtils

internal object AndroidAttachBridge {
    @JvmStatic
    fun clients(project: Project?, query: Map<String, String>): Response {
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            StudioThreading.onIdeaThread {
                val payload = matchingClients(project, query).map { clientInfo(it) }
                BridgeProtocol.ok("ok", true, "project", projectInfo(project), "clients", payload)
            }
        } catch (t: Throwable) {
            BridgeProtocol.bad(t.message)
        }
    }

    @JvmStatic
    fun attach(project: Project?, query: Map<String, String>): Response {
        if (BridgeProtocol.booleanParam(query, BridgeQuery.DIALOG, false)) {
            return openDialog(project)
        }
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            StudioThreading.onIdeaThread {
                val selected = selectAttachClient(project, query)
                val runConfiguration = selectRunConfiguration(project, query)
                val mode = query[BridgeQuery.MODE]?.lowercase()?.takeUnless { it.isBlank() } ?: "auto"
                if ((mode == "native" || mode == "mixed") && !selected.client.clientData.isNativeDebuggable) {
                    throw IllegalArgumentException("selected process is not native-debuggable; use --mode java or rebuild with native debugging enabled")
                }
                val debugger = selectAndroidDebugger(project, query, mode)
                    ?: throw IllegalStateException("no supported Android debugger")

                AndroidConnectDebugger.closeOldSessionAndRun(project, debugger, selected.client, runConfiguration)
                BridgeProtocol.ok(
                    "ok", true,
                    "action", BridgeValues.ACTION_ATTACH,
                    "project", projectInfo(project),
                    "client", clientInfo(selected),
                    "requested_mode", mode,
                    "debugger", debuggerInfo(debugger),
                    "run_configuration", runConfigurationInfo(runConfiguration),
                )
            }
        } catch (t: Throwable) {
            BridgeProtocol.bad(t.message)
        }
    }

    private fun openDialog(project: Project?): Response {
        if (project == null) return BridgeProtocol.bad("no project")
        val action = ActionManager.getInstance().getAction(BridgeValues.ANDROID_CONNECT_DEBUGGER_ACTION)
            ?: return BridgeProtocol.bad("${BridgeValues.ANDROID_CONNECT_DEBUGGER_ACTION} is not available")
        ApplicationManager.getApplication().invokeLater {
            val dataContext = SimpleDataContext.builder()
                .add(CommonDataKeys.PROJECT, project)
                .build()
            @Suppress("DEPRECATION")
            val event = AnActionEvent.createFromAnAction(action, null, ActionPlaces.UNKNOWN, dataContext)
            action.actionPerformed(event)
        }
        return BridgeProtocol.ok("ok", true, "action", BridgeValues.ANDROID_CONNECT_DEBUGGER_ACTION, "project", projectInfo(project))
    }

    private fun selectAttachClient(project: Project, query: Map<String, String>): AttachClient {
        val candidates = matchingClients(project, query)
        if (candidates.isEmpty()) {
            throw IllegalArgumentException("no matching Android process; use debugger clients to inspect attachable processes")
        }
        if (candidates.size > 1) {
            throw IllegalArgumentException("multiple matching Android processes; pass package, pid, or device")
        }
        return candidates.single()
    }

    private fun matchingClients(project: Project, query: Map<String, String>): List<AttachClient> {
        val bridge: AndroidDebugBridge = AndroidSdkUtils.getDebugBridge(project)
            ?: throw IllegalStateException("Android debug bridge is not available for project")
        val requestedDevice = query[BridgeQuery.DEVICE]
        val requestedPackage = query[BridgeQuery.PACKAGE]
        val requestedPid = optionalInt(query[BridgeQuery.PID])

        val candidates = mutableListOf<AttachClient>()
        for (device in bridge.devices) {
            if (!deviceMatches(device, requestedDevice)) continue
            if (!device.isOnline) continue
            for (client in device.clients) {
                if (!clientMatches(client, requestedPackage, requestedPid)) continue
                candidates += AttachClient(device, client)
            }
        }
        return candidates
    }

    private fun deviceMatches(device: IDevice, requestedDevice: String?): Boolean {
        if (requestedDevice.isNullOrBlank()) return true
        return requestedDevice == device.serialNumber || requestedDevice == avdName(device)
    }

    private fun clientMatches(client: Client, requestedPackage: String?, requestedPid: Int?): Boolean {
        if (!client.isValid) return false
        val data = client.clientData
        if (requestedPid != null && data.pid != requestedPid) return false
        if (requestedPackage.isNullOrBlank()) return true
        val packageName = data.packageName
        val processName = data.processName
        return requestedPackage == packageName ||
            requestedPackage == processName ||
            processName?.startsWith("$requestedPackage:") == true
    }

    private fun optionalInt(value: String?): Int? {
        if (value.isNullOrBlank()) return null
        return value.toIntOrNull() ?: throw IllegalArgumentException("invalid integer: $value")
    }

    private fun selectRunConfiguration(project: Project, query: Map<String, String>): RunConfigurationWithDebugger? {
        val requested = query[BridgeQuery.CONFIGURATION]
        val runManager = RunManager.getInstance(project)
        if (!requested.isNullOrBlank()) {
            for (configuration: RunConfiguration in runManager.allConfigurationsList) {
                if (configuration is RunConfigurationWithDebugger && requested == configuration.name) {
                    return configuration
                }
            }
            throw IllegalArgumentException("Android run configuration not found: $requested")
        }

        val selected: RunnerAndConfigurationSettings? = runManager.selectedConfiguration
        val configuration = selected?.configuration
        return configuration as? RunConfigurationWithDebugger
    }

    private fun selectAndroidDebugger(project: Project, query: Map<String, String>, mode: String): AndroidDebugger<*>? {
        val requested = query[BridgeQuery.DEBUGGER]
        var fallback: AndroidDebugger<*>? = null
        var defaultDebugger: AndroidDebugger<*>? = null
        var modeMatch: AndroidDebugger<*>? = null
        for (debugger in AndroidDebugger.EP_NAME.extensionList) {
            if (!debugger.supportsProject(project)) continue
            if (fallback == null) fallback = debugger
            if (debugger.shouldBeDefault()) defaultDebugger = debugger
            if (!requested.isNullOrBlank() && (requested == debugger.id || requested == debugger.displayName)) {
                return debugger
            }
            if (modeMatch == null && debuggerMatchesMode(debugger, mode)) {
                modeMatch = debugger
            }
        }
        if (!requested.isNullOrBlank()) {
            throw IllegalArgumentException("Android debugger not found: $requested")
        }
        return modeMatch ?: defaultDebugger ?: fallback
    }

    private fun debuggerMatchesMode(debugger: AndroidDebugger<*>, mode: String): Boolean {
        if (mode == "auto") return false
        val text = "${debugger.id} ${debugger.displayName}".lowercase()
        return when (mode) {
            "java" -> debugger.shouldBeDefault() || text.contains("java")
            "native" -> text.contains("native") || text.contains("lldb")
            "mixed" -> text.contains("native") || text.contains("lldb") || text.contains("dual")
            else -> false
        }
    }

    private fun clientInfo(attachClient: AttachClient): Map<String, Any?> {
        val client = attachClient.client
        val data: ClientData = client.clientData
        return BridgeProtocol.map(
            "device", deviceInfo(attachClient.device),
            "pid", data.pid,
            "package", data.packageName,
            "process", data.processName,
            "vm_identifier", data.vmIdentifier,
            "abi", data.abi,
            "native_debuggable", data.isNativeDebuggable,
            "debugger_attached", client.isDebuggerAttached,
            "debugger_port", client.debuggerListenPort,
            "debugger_status", data.debuggerConnectionStatus?.name,
            "valid", client.isValid,
        )
    }

    private fun deviceInfo(device: IDevice): Map<String, Any?> =
        BridgeProtocol.map(
            "serial", device.serialNumber,
            "avd", avdName(device),
            "state", device.state?.name,
            "online", device.isOnline,
            "emulator", device.isEmulator,
        )

    @Suppress("DEPRECATION")
    private fun avdName(device: IDevice): String? = device.avdName

    private fun debuggerInfo(debugger: AndroidDebugger<*>): Map<String, Any?> =
        BridgeProtocol.map(
            "id", debugger.id,
            "display_name", debugger.displayName,
            "default", debugger.shouldBeDefault(),
            "capabilities", BridgeProtocol.map(
                "semantic_mode_selection", true,
            ),
        )

    private fun runConfigurationInfo(runConfiguration: RunConfigurationWithDebugger?): Map<String, Any?> {
        if (runConfiguration == null) return BridgeProtocol.map("source", "default")
        return BridgeProtocol.map(
            "source", "run_configuration",
            "name", runConfiguration.name,
            "type", runConfiguration.type.displayName,
        )
    }

    private fun projectInfo(project: Project): Map<String, Any?> =
        BridgeProtocol.map(
            "name", project.name,
            "base_path", project.basePath,
            "disposed", project.isDisposed,
        )

    private data class AttachClient(
        val device: IDevice,
        val client: Client,
    )
}
