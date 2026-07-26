package io.github.andriyo.shadowdroid.sample

import android.graphics.Color
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge

class DetailActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        val source = intent.getStringExtra("source") ?: "unknown"
        setContent {
            ShadowLabTheme {
                LabDestinationScreen(
                    rootTag = "detail_root",
                    eyebrow = "INCIDENT DETAIL",
                    title = "Dispatch received.",
                    summary =
                        "A separate Activity verifies explicit lifecycle transitions and a deterministic return path.",
                    accent = LabViolet,
                ) {
                    DestinationValue(
                        label = "NAVIGATION SOURCE",
                        value = "Detail activity opened from $source",
                        tag = "detail_message",
                        description = "Detail activity message",
                        accent = LabVioletBright,
                    )
                    DestinationValue(
                        label = "TASK STATE",
                        value = "Foreground · return will finish this Activity",
                        accent = LabMint,
                    )
                    DestinationButton(
                        label = "Finish detail",
                        tag = "detail_finish_button",
                        description = "Finish detail activity button",
                        onClick = ::finish,
                    )
                }
            }
        }
    }
}
