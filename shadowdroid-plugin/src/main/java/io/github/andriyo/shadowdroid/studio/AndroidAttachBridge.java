package io.github.andriyo.shadowdroid.studio;

import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.bad;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.booleanParam;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.map;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.ok;
import static io.github.andriyo.shadowdroid.studio.StudioThreading.onIdeaThread;

import com.android.ddmlib.AndroidDebugBridge;
import com.android.ddmlib.Client;
import com.android.ddmlib.ClientData;
import com.android.ddmlib.IDevice;
import com.android.tools.idea.execution.common.debug.AndroidDebugger;
import com.android.tools.idea.execution.common.debug.RunConfigurationWithDebugger;
import com.android.tools.idea.execution.common.debug.utils.AndroidConnectDebugger;
import com.intellij.execution.RunManager;
import com.intellij.execution.RunnerAndConfigurationSettings;
import com.intellij.execution.configurations.RunConfiguration;
import com.intellij.openapi.actionSystem.ActionManager;
import com.intellij.openapi.actionSystem.ActionPlaces;
import com.intellij.openapi.actionSystem.AnAction;
import com.intellij.openapi.actionSystem.AnActionEvent;
import com.intellij.openapi.actionSystem.CommonDataKeys;
import com.intellij.openapi.actionSystem.DataContext;
import com.intellij.openapi.actionSystem.impl.SimpleDataContext;
import com.intellij.openapi.application.ApplicationManager;
import com.intellij.openapi.project.Project;
import org.jetbrains.android.sdk.AndroidSdkUtils;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;

final class AndroidAttachBridge {
    private AndroidAttachBridge() {
    }

    static Response clients(Project project, Map<String, String> query) {
        if (project == null) return bad("no project");
        try {
            return onIdeaThread(() -> {
                List<Object> payload = new ArrayList<>();
                for (AttachClient candidate : matchingClients(project, query)) {
                    payload.add(clientInfo(candidate));
                }
                return ok("ok", true, "project", projectInfo(project), "clients", payload);
            });
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    static Response attach(Project project, Map<String, String> query) {
        if (booleanParam(query, "dialog", false)) {
            return openDialog(project);
        }

        if (project == null) return bad("no project");
        try {
            return onIdeaThread(() -> {
                AttachClient selected = selectAttachClient(project, query);
                RunConfigurationWithDebugger runConfiguration = selectRunConfiguration(project, query);
                AndroidDebugger<?> debugger = selectAndroidDebugger(project, query);
                if (debugger == null) throw new IllegalStateException("no supported Android debugger");

                AndroidConnectDebugger.closeOldSessionAndRun(project, debugger, selected.client, runConfiguration);
                return ok(
                    "ok", true,
                    "action", "attach",
                    "project", projectInfo(project),
                    "client", clientInfo(selected),
                    "debugger", debuggerInfo(debugger),
                    "run_configuration", runConfigurationInfo(runConfiguration)
                );
            });
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    private static Response openDialog(Project project) {
        if (project == null) return bad("no project");
        AnAction action = ActionManager.getInstance().getAction("AndroidConnectDebuggerAction");
        if (action == null) return bad("AndroidConnectDebuggerAction is not available");
        ApplicationManager.getApplication().invokeLater(() -> {
            DataContext dataContext = SimpleDataContext.builder()
                .add(CommonDataKeys.PROJECT, project)
                .build();
            AnActionEvent event = AnActionEvent.createFromAnAction(action, null, ActionPlaces.UNKNOWN, dataContext);
            action.actionPerformed(event);
        });
        return ok("ok", true, "action", "AndroidConnectDebuggerAction", "project", projectInfo(project));
    }

    private static AttachClient selectAttachClient(Project project, Map<String, String> query) {
        List<AttachClient> candidates = matchingClients(project, query);
        if (candidates.isEmpty()) {
            throw new IllegalArgumentException("no matching Android process; use debugger clients to inspect attachable processes");
        }
        if (candidates.size() > 1) {
            throw new IllegalArgumentException("multiple matching Android processes; pass package, pid, or device");
        }
        return candidates.get(0);
    }

    private static List<AttachClient> matchingClients(Project project, Map<String, String> query) {
        AndroidDebugBridge bridge = AndroidSdkUtils.getDebugBridge(project);
        if (bridge == null) {
            throw new IllegalStateException("Android debug bridge is not available for project");
        }
        String requestedDevice = query.get("device");
        String requestedPackage = query.get("package");
        Integer requestedPid = optionalInt(query.get("pid"));

        List<AttachClient> candidates = new ArrayList<>();
        for (IDevice device : bridge.getDevices()) {
            if (!deviceMatches(device, requestedDevice)) continue;
            if (!device.isOnline()) continue;
            for (Client client : device.getClients()) {
                if (!clientMatches(client, requestedPackage, requestedPid)) continue;
                candidates.add(new AttachClient(device, client));
            }
        }
        return candidates;
    }

    private static boolean deviceMatches(IDevice device, String requestedDevice) {
        if (requestedDevice == null || requestedDevice.isBlank()) return true;
        return requestedDevice.equals(device.getSerialNumber()) || requestedDevice.equals(device.getAvdName());
    }

    private static boolean clientMatches(Client client, String requestedPackage, Integer requestedPid) {
        if (!client.isValid()) return false;
        ClientData data = client.getClientData();
        if (requestedPid != null && data.getPid() != requestedPid) return false;
        if (requestedPackage == null || requestedPackage.isBlank()) return true;
        String packageName = data.getPackageName();
        String processName = data.getProcessName();
        return requestedPackage.equals(packageName)
            || requestedPackage.equals(processName)
            || (processName != null && processName.startsWith(requestedPackage + ":"));
    }

    private static Integer optionalInt(String value) {
        if (value == null || value.isBlank()) return null;
        try {
            return Integer.parseInt(value);
        } catch (NumberFormatException e) {
            throw new IllegalArgumentException("invalid integer: " + value);
        }
    }

    private static RunConfigurationWithDebugger selectRunConfiguration(Project project, Map<String, String> query) {
        String requested = query.get("configuration");
        RunManager runManager = RunManager.getInstance(project);
        if (requested != null && !requested.isBlank()) {
            for (RunConfiguration configuration : runManager.getAllConfigurationsList()) {
                if (configuration instanceof RunConfigurationWithDebugger withDebugger
                    && requested.equals(configuration.getName())) {
                    return withDebugger;
                }
            }
            throw new IllegalArgumentException("Android run configuration not found: " + requested);
        }

        RunnerAndConfigurationSettings selected = runManager.getSelectedConfiguration();
        if (selected != null && selected.getConfiguration() instanceof RunConfigurationWithDebugger withDebugger) {
            return withDebugger;
        }
        return null;
    }

    private static AndroidDebugger<?> selectAndroidDebugger(Project project, Map<String, String> query) {
        String requested = query.get("debugger");
        AndroidDebugger<?> fallback = null;
        AndroidDebugger<?> defaultDebugger = null;
        for (AndroidDebugger<?> debugger : AndroidDebugger.EP_NAME.getExtensionList()) {
            if (!debugger.supportsProject(project)) continue;
            if (fallback == null) fallback = debugger;
            if (debugger.shouldBeDefault()) defaultDebugger = debugger;
            if (requested != null && !requested.isBlank()
                && (requested.equals(debugger.getId()) || requested.equals(debugger.getDisplayName()))) {
                return debugger;
            }
        }
        if (requested != null && !requested.isBlank()) {
            throw new IllegalArgumentException("Android debugger not found: " + requested);
        }
        return defaultDebugger != null ? defaultDebugger : fallback;
    }

    private static Map<String, Object> clientInfo(AttachClient attachClient) {
        Client client = attachClient.client;
        ClientData data = client.getClientData();
        return map(
            "device", deviceInfo(attachClient.device),
            "pid", data.getPid(),
            "package", data.getPackageName(),
            "process", data.getProcessName(),
            "vm_identifier", data.getVmIdentifier(),
            "abi", data.getAbi(),
            "native_debuggable", data.isNativeDebuggable(),
            "debugger_attached", client.isDebuggerAttached(),
            "debugger_port", client.getDebuggerListenPort(),
            "debugger_status", data.getDebuggerConnectionStatus() == null ? null : data.getDebuggerConnectionStatus().name(),
            "valid", client.isValid()
        );
    }

    private static Map<String, Object> deviceInfo(IDevice device) {
        return map(
            "serial", device.getSerialNumber(),
            "avd", device.getAvdName(),
            "state", device.getState() == null ? null : device.getState().name(),
            "online", device.isOnline(),
            "emulator", device.isEmulator()
        );
    }

    private static Map<String, Object> debuggerInfo(AndroidDebugger<?> debugger) {
        return map(
            "id", debugger.getId(),
            "display_name", debugger.getDisplayName(),
            "default", debugger.shouldBeDefault()
        );
    }

    private static Map<String, Object> runConfigurationInfo(RunConfigurationWithDebugger runConfiguration) {
        if (runConfiguration == null) {
            return map("source", "default");
        }
        return map(
            "source", "run_configuration",
            "name", runConfiguration.getName(),
            "type", runConfiguration.getType().getDisplayName()
        );
    }

    private static Map<String, Object> projectInfo(Project project) {
        return map(
            "name", project.getName(),
            "base_path", project.getBasePath(),
            "disposed", project.isDisposed()
        );
    }

    private record AttachClient(IDevice device, Client client) {
    }
}
