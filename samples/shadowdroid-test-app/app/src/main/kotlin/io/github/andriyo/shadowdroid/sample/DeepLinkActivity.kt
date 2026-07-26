package io.github.andriyo.shadowdroid.sample

import android.graphics.Color
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge

class DeepLinkActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        val uri = intent.data?.toString() ?: "none"
        setContent {
            ShadowLabTheme {
                LabDestinationScreen(
                    rootTag = "deep_link_root",
                    eyebrow = "EXTERNAL HANDOFF",
                    title = "Link resolved.",
                    summary =
                        "A browsable custom scheme landed in an exported Activity with its payload intact.",
                    accent = LabCyan,
                ) {
                    DestinationValue(
                        label = "RESOLVED URI",
                        value = "Deep link: $uri",
                        tag = "deep_link_message",
                        description = "Deep link activity message",
                        accent = LabCyan,
                    )
                    DestinationValue(
                        label = "ROUTE CONTRACT",
                        value = "shadowdroid://sample/deeplink/…",
                        accent = LabMint,
                    )
                    DestinationButton(
                        label = "Finish deep link",
                        tag = "deep_link_finish_button",
                        description = "Finish deep link activity button",
                        onClick = ::finish,
                    )
                }
            }
        }
    }
}
