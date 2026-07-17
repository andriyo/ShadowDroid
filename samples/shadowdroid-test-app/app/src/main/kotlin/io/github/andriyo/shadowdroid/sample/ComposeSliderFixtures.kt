package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ComposeView
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp

fun composeSliderFixtures(
    activity: Activity,
    onStatus: (String) -> Unit,
): ComposeView =
    ComposeView(activity).apply {
        id = R.id.compose_slider_fixtures
        setContent {
            MaterialTheme {
                SliderFixtures(onStatus)
            }
        }
    }

@Composable
@OptIn(ExperimentalComposeUiApi::class)
private fun SliderFixtures(onStatus: (String) -> Unit) {
    var continuous by remember { mutableStateOf(0.38f) }
    var discrete by remember { mutableStateOf(50f) }
    var rtl by remember { mutableStateOf(25f) }

    Column(
        Modifier
            .semantics { testTagsAsResourceId = true }
            .padding(8.dp),
    ) {
        Text("Compose continuous")
        Slider(
            value = continuous,
            onValueChange = { continuous = it },
            onValueChangeFinished = { onStatus("Compose continuous slider changed to $continuous") },
            valueRange = 0.22f..0.50f,
            modifier =
                Modifier
                    .testTag("compose_continuous_slider")
                    .semantics { contentDescription = "Compose continuous slider" },
        )

        Text("Compose discrete")
        Slider(
            value = discrete,
            onValueChange = { discrete = it },
            onValueChangeFinished = { onStatus("Compose discrete slider changed to $discrete") },
            valueRange = 0f..100f,
            steps = 3,
            modifier =
                Modifier
                    .testTag("compose_discrete_slider")
                    .semantics { contentDescription = "Compose discrete slider" },
        )

        Text("Compose disabled")
        Slider(
            value = 40f,
            onValueChange = {},
            enabled = false,
            valueRange = 0f..100f,
            modifier =
                Modifier
                    .testTag("compose_disabled_slider")
                    .semantics { contentDescription = "Compose disabled slider" },
        )

        Text("Compose RTL")
        CompositionLocalProvider(LocalLayoutDirection provides LayoutDirection.Rtl) {
            Slider(
                value = rtl,
                onValueChange = { rtl = it },
                onValueChangeFinished = { onStatus("Compose RTL slider changed to $rtl") },
                valueRange = 0f..100f,
                modifier =
                    Modifier
                        .testTag("compose_rtl_slider")
                        .semantics { contentDescription = "Compose RTL slider" },
            )
        }
    }
}
