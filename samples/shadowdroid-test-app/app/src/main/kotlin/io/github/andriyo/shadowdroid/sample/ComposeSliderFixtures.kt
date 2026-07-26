package io.github.andriyo.shadowdroid.sample

import android.content.Context
import android.widget.FrameLayout
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.SliderDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
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
import kotlinx.coroutines.delay

/**
 * Retained for callers that need a View-hosts-Compose interop fixture. The main
 * Field Lab also renders [ComposeRangeFixtures] directly inside its Compose
 * screen, giving Layout Inspector both interop and native Compose paths.
 */
fun composeSliderFixtures(
    activity: Context,
    onStatus: (String) -> Unit,
): FrameLayout {
    val container = FrameLayout(activity).apply { id = R.id.compose_slider_fixtures }
    val composeView =
        ComposeView(activity).apply {
            setContent {
                ShadowLabTheme {
                    ComposeRangeFixtures(onStatus = onStatus)
                }
            }
        }
    container.addView(
        composeView,
        FrameLayout.LayoutParams(
            FrameLayout.LayoutParams.MATCH_PARENT,
            FrameLayout.LayoutParams.MATCH_PARENT,
        ),
    )
    return container
}

@Composable
@OptIn(ExperimentalComposeUiApi::class, ExperimentalMaterial3Api::class)
fun ComposeRangeFixtures(
    onStatus: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    var continuous by remember { mutableFloatStateOf(0.38f) }
    var discrete by remember { mutableFloatStateOf(50f) }
    var rtl by remember { mutableFloatStateOf(25f) }
    var delayedDestination by remember { mutableStateOf(false) }
    var navigationPending by remember { mutableStateOf(false) }

    LaunchedEffect(navigationPending) {
        if (navigationPending) {
            onStatus("Compose delayed navigation scheduled")
            delay(DELAYED_COMPOSE_NAVIGATION_MS)
            onStatus("Compose delayed destination ready")
            delayedDestination = true
            navigationPending = false
        }
    }

    AnimatedContent(
        targetState = delayedDestination,
        modifier =
            modifier
                .fillMaxWidth()
                .semantics { testTagsAsResourceId = true }
                .testTag("compose_slider_fixtures"),
        transitionSpec = {
            (slideInHorizontally { it / 3 } + fadeIn()) togetherWith
                (slideOutHorizontally { -it / 3 } + fadeOut())
        },
        label = "compose-range-destination",
    ) { showingDestination ->
        if (showingDestination) {
            DelayedDestination(
                onReturn = {
                    delayedDestination = false
                    onStatus("Returned to Compose controls")
                },
            )
        } else {
            Column(verticalArrangement = Arrangement.spacedBy(14.dp)) {
                RangeHeader(
                    eyebrow = "COMPOSE · CONTINUOUS",
                    value = "%.3f".format(continuous),
                )
                Slider(
                    value = continuous,
                    onValueChange = { continuous = it },
                    onValueChangeFinished = {
                        onStatus("Compose continuous slider changed to $continuous")
                    },
                    valueRange = 0.22f..0.50f,
                    colors = labSliderColors(),
                    thumb = { InspectorSafeSliderThumb(enabled = true) },
                    modifier =
                        Modifier
                            .testTag("compose_continuous_slider")
                            .semantics { contentDescription = "Compose continuous slider" },
                )

                RangeHeader(
                    eyebrow = "COMPOSE · DISCRETE",
                    value = discrete.toInt().toString(),
                )
                Slider(
                    value = discrete,
                    onValueChange = { discrete = it },
                    onValueChangeFinished = {
                        onStatus("Compose discrete slider changed to $discrete")
                    },
                    valueRange = 0f..100f,
                    steps = 3,
                    colors = labSliderColors(),
                    thumb = { InspectorSafeSliderThumb(enabled = true) },
                    modifier =
                        Modifier
                            .testTag("compose_discrete_slider")
                            .semantics { contentDescription = "Compose discrete slider" },
                )

                RangeHeader(eyebrow = "COMPOSE · DISABLED", value = "40")
                Slider(
                    value = 40f,
                    onValueChange = {},
                    enabled = false,
                    valueRange = 0f..100f,
                    colors = labSliderColors(),
                    thumb = { InspectorSafeSliderThumb(enabled = false) },
                    modifier =
                        Modifier
                            .testTag("compose_disabled_slider")
                            .semantics { contentDescription = "Compose disabled slider" },
                )

                RangeHeader(eyebrow = "COMPOSE · RTL", value = rtl.toInt().toString())
                CompositionLocalProvider(LocalLayoutDirection provides LayoutDirection.Rtl) {
                    Slider(
                        value = rtl,
                        onValueChange = { rtl = it },
                        onValueChangeFinished = {
                            onStatus("Compose RTL slider changed to $rtl")
                        },
                        valueRange = 0f..100f,
                        colors = labSliderColors(),
                        thumb = { InspectorSafeSliderThumb(enabled = true) },
                        modifier =
                            Modifier
                                .testTag("compose_rtl_slider")
                                .semantics { contentDescription = "Compose RTL slider" },
                    )
                }

                Button(
                    onClick = { navigationPending = true },
                    enabled = !navigationPending,
                    colors =
                        ButtonDefaults.buttonColors(
                            containerColor = MaterialTheme.colorScheme.primaryContainer,
                            contentColor = MaterialTheme.colorScheme.onPrimaryContainer,
                        ),
                    modifier =
                        Modifier
                            .fillMaxWidth()
                            .testTag("compose_delayed_navigation")
                            .semantics {
                                contentDescription = "Open delayed Compose destination"
                            },
                ) {
                    Text(if (navigationPending) "Opening destination…" else "Open delayed destination")
                }
            }
        }
    }
}

@Composable
private fun RangeHeader(eyebrow: String, value: String) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(
            text = eyebrow,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(
            text = value,
            style = MaterialTheme.typography.labelSmall,
            color = LabCyan,
        )
    }
}

@Composable
@OptIn(ExperimentalMaterial3Api::class)
private fun labSliderColors() =
    SliderDefaults.colors(
        thumbColor = LabVioletBright,
        activeTrackColor = LabViolet,
        inactiveTrackColor = LabOutline,
        activeTickColor = LabText,
        inactiveTickColor = LabTextMuted,
        disabledThumbColor = LabTextMuted,
        disabledActiveTrackColor = LabOutline,
        disabledInactiveTrackColor = LabPanel,
    )

@Composable
fun InspectorSafeSliderThumb(enabled: Boolean) {
    // Compose UI Inspector 1.11.x can mis-map SliderDefaults.Thumb's Boolean
    // and inline DpSize parameters and crash the inspected process. Keep this
    // custom primitive thumb until that Inspector path is revalidated live.
    val color =
        if (enabled) {
            LabVioletBright
        } else {
            LabTextMuted.copy(alpha = 0.55f)
        }
    Box(
        Modifier
            .size(24.dp)
            .background(color, CircleShape)
            .padding(6.dp),
    ) {
        Box(
            Modifier
                .size(12.dp)
                .background(LabInk.copy(alpha = 0.45f), CircleShape),
        )
    }
}

@Composable
private fun DelayedDestination(onReturn: () -> Unit) {
    Column(
        modifier =
            Modifier
                .fillMaxWidth()
                .testTag("compose_delayed_destination")
                .background(LabPanelRaised, RoundedCornerShape(20.dp))
                .padding(22.dp),
    ) {
        Text(
            "Destination acquired",
            style = MaterialTheme.typography.titleLarge,
        )
        Spacer(Modifier.height(6.dp))
        Text(
            "The same Activity replaced this subtree after a deterministic 350 ms transition.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(18.dp))
        Button(
            onClick = onReturn,
            modifier =
                Modifier
                    .fillMaxWidth()
                    .testTag("compose_return_from_destination")
                    .semantics { contentDescription = "Return from Compose destination" },
        ) {
            Text("Return to calibration")
        }
    }
}

private const val DELAYED_COMPOSE_NAVIGATION_MS = 350L
