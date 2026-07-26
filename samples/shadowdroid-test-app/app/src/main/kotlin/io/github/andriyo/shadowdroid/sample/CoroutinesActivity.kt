package io.github.andriyo.shadowdroid.sample

import android.graphics.Color
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/**
 * Launches the [CoroutineWorkload] zoo on open, so simply starting this activity
 * makes the app's coroutine state worth dumping with `shadowdroid aar coroutines`.
 */
class CoroutinesActivity : ComponentActivity() {
    private var status by mutableStateOf("")

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        status = CoroutineWorkload.startOnce()
        setContent {
            ShadowLabTheme {
                LabDestinationScreen(
                    rootTag = "coroutines_root",
                    eyebrow = "RUNTIME OBSERVATORY",
                    title = "Telemetry workers.",
                    summary =
                        "A deliberately unhealthy coroutine topology is now live behind this calm status surface.",
                    accent = LabMint,
                ) {
                    DestinationValue(
                        label = "WORKLOAD STATUS",
                        value = status,
                        tag = "coroutines_status",
                        description = "Coroutine workload status",
                        accent = LabMint,
                    )
                    DestinationValue(
                        label = "EXPECTED TOPOLOGY",
                        value = "leaked heartbeat · idle workers · slow collector · blocked emitter",
                        description = "Coroutine workload title",
                        accent = LabAmber,
                    )
                    DestinationButton(
                        label = "Spawn another worker",
                        tag = "coroutines_spawn_button",
                        description = "Spawn coroutine worker button",
                    ) {
                        status = CoroutineWorkload.spawnWorker()
                    }
                }
            }
        }
    }
}
