package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import android.view.View
import android.widget.FrameLayout
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
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
): FrameLayout {
    val container = FrameLayout(activity).apply { id = R.id.compose_slider_fixtures }
    val controls = ComposeView(activity)
    val destination = ComposeView(activity).apply { visibility = View.GONE }
    val childLayout =
        FrameLayout.LayoutParams(
            FrameLayout.LayoutParams.MATCH_PARENT,
            FrameLayout.LayoutParams.MATCH_PARENT,
        )
    destination.setContent {
        MaterialTheme {
            DelayedDestination {
                destination.visibility = View.GONE
                controls.visibility = View.VISIBLE
            }
        }
    }
    controls.setContent {
        MaterialTheme {
            SliderFixtures(
                onStatus = onStatus,
                onDelayedNavigation = {
                    onStatus("Compose delayed navigation scheduled")
                    controls.postDelayed(
                        {
                            onStatus("Compose delayed destination ready")
                            controls.visibility = View.GONE
                            destination.visibility = View.VISIBLE
                        },
                        DELAYED_COMPOSE_NAVIGATION_MS,
                    )
                },
            )
        }
    }
    container.addView(controls, childLayout)
    container.addView(destination, childLayout)
    return container
}

@Composable
@OptIn(ExperimentalComposeUiApi::class)
private fun SliderFixtures(
    onStatus: (String) -> Unit,
    onDelayedNavigation: () -> Unit,
) {
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

        Button(
            onClick = onDelayedNavigation,
            modifier =
                Modifier
                    .testTag("compose_delayed_navigation")
                    .semantics { contentDescription = "Open delayed Compose destination" },
        ) {
            Text("Open delayed Compose destination")
        }
    }
}

@Composable
@OptIn(ExperimentalComposeUiApi::class)
private fun DelayedDestination(onReturn: () -> Unit) {
    Column(
        Modifier
            .semantics { testTagsAsResourceId = true }
            .padding(8.dp),
    ) {
        Text(
            "Compose delayed destination ready",
            Modifier.testTag("compose_delayed_destination"),
        )
        Button(
            onClick = onReturn,
            modifier =
                Modifier
                    .testTag("compose_return_from_destination")
                    .semantics { contentDescription = "Return from Compose destination" },
        ) {
            Text("Return to Compose controls")
        }
    }
}

private const val DELAYED_COMPOSE_NAVIGATION_MS = 350L
