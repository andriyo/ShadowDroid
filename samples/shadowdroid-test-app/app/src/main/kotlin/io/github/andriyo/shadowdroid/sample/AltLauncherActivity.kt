package io.github.andriyo.shadowdroid.sample

import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics

class AltLauncherActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        setContent {
            ShadowLabTheme {
                LabDestinationScreen(
                    rootTag = "alt_root",
                    eyebrow = "QUICK DIAGNOSTIC",
                    title = "A second way in.",
                    summary =
                        "This alternate launcher intentionally keeps Android activity resolution and debugger selection non-trivial.",
                    accent = LabAmber,
                ) {
                    Text(
                        "Alternate launcher activity",
                        style = MaterialTheme.typography.titleLarge,
                        modifier =
                            Modifier.semantics {
                                contentDescription = "Alternate launcher activity message"
                            },
                    )
                    DestinationValue(
                        label = "ENTRY POINT",
                        value = "AltLauncherActivity · exported launcher",
                        accent = LabAmber,
                    )
                    DestinationButton(
                        label = "Open full Field Lab",
                        tag = "alt_open_main_button",
                        description = "Open main test screen button",
                    ) {
                        startActivity(Intent(this@AltLauncherActivity, MainActivity::class.java))
                    }
                }
            }
        }
    }
}
