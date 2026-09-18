package example.verification;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Intent;
import android.os.ParcelFileDescriptor;
import android.widget.EditText;
import android.widget.TextView;
import androidx.test.ext.junit.runners.AndroidJUnit4;
import androidx.test.platform.app.InstrumentationRegistry;
import org.junit.*;
import org.junit.runner.RunWith;
import java.io.FileInputStream;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import static org.junit.Assert.*;

@RunWith(AndroidJUnit4.class)
public class AcceptanceTest {
    private final Instrumentation instrumentation = InstrumentationRegistry.getInstrumentation();
    private Activity activity;
    private String originalNight;
    private String shell(String command) throws Exception {
        try (ParcelFileDescriptor fd = instrumentation.getUiAutomation().executeShellCommand(command);
             FileInputStream stream = new FileInputStream(fd.getFileDescriptor())) {
            return new String(stream.readAllBytes(), StandardCharsets.UTF_8).trim();
        }
    }
    @Before public void setup() throws Exception {
        originalNight = shell("cmd uimode night").replace("Night mode: ", "");
    }
    private void launch() {
        activity = instrumentation.startActivitySync(new Intent(instrumentation.getTargetContext(), MainActivity.class)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TASK));
        instrumentation.waitForIdleSync();
    }
    @After public void cleanup() throws Exception {
        if (activity != null) instrumentation.runOnMainSync(() -> activity.finish());
        shell("cmd uimode night " + originalNight);
    }
    @Test public void titleAndEditingRegression() {
        launch();
        String value = UUID.randomUUID().toString();
        instrumentation.runOnMainSync(() -> {
            assertEquals("Draft editor", ((TextView) activity.findViewById(R.id.title)).getText().toString());
            EditText draft = activity.findViewById(R.id.draft);
            draft.setText(value);
            assertEquals(value, draft.getText().toString());
        });
    }
    @Test public void draftSurvivesRecreation() {
        launch();
        String value = "draft-" + UUID.randomUUID();
        instrumentation.runOnMainSync(() -> ((EditText) activity.findViewById(R.id.draft)).setText(value));
        Instrumentation.ActivityMonitor monitor = instrumentation.addMonitor(MainActivity.class.getName(), null, false);
        try {
            instrumentation.runOnMainSync(() -> activity.recreate());
            Activity recreated = monitor.waitForActivityWithTimeout(5000);
            assertNotNull("no recreated activity", recreated);
            assertNotSame(activity, recreated);
            activity = recreated;
            instrumentation.waitForIdleSync();
            instrumentation.runOnMainSync(() -> assertEquals(value, ((EditText) activity.findViewById(R.id.draft)).getText().toString()));
        } finally { instrumentation.removeMonitor(monitor); }
    }
    @Test public void darkThemeReflectsSystem() throws Exception {
        shell("cmd uimode night yes");
        launch();
        instrumentation.runOnMainSync(() -> assertEquals("Dark", ((TextView) activity.findViewById(R.id.theme)).getText().toString()));
        instrumentation.runOnMainSync(() -> activity.finish());
        shell("cmd uimode night no");
        launch();
        instrumentation.runOnMainSync(() -> assertEquals("Light", ((TextView) activity.findViewById(R.id.theme)).getText().toString()));
    }
    @Test public void secondaryRoute() {
        launch();
        Instrumentation.ActivityMonitor monitor = instrumentation.addMonitor(MainActivity.DetailActivity.class.getName(), null, false);
        try {
            instrumentation.runOnMainSync(() -> activity.findViewById(R.id.next).performClick());
            Activity detail = monitor.waitForActivityWithTimeout(5000);
            assertNotNull("Details button never reached detail activity", detail);
            activity = detail;
            instrumentation.waitForIdleSync();
            instrumentation.runOnMainSync(() -> assertEquals("Details ready", ((TextView) activity.findViewById(R.id.detail)).getText().toString()));
        } finally { instrumentation.removeMonitor(monitor); }
    }
}
