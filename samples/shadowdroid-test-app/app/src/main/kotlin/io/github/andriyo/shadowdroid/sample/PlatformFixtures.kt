package io.github.andriyo.shadowdroid.sample

import android.content.Context
import android.content.res.ColorStateList
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.text.Editable
import android.text.InputType
import android.text.TextWatcher
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.SeekBar
import android.widget.TextView
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView
import kotlin.math.roundToInt

@Composable
fun PlatformTextField(
    idValue: Int,
    value: String,
    label: String,
    description: String,
    onValueChanged: (String) -> Unit,
    modifier: Modifier = Modifier,
    multiline: Boolean = false,
) {
    val currentOnValueChanged by rememberUpdatedState(onValueChanged)

    AndroidView(
        modifier = modifier.fillMaxWidth(),
        factory = { context ->
            FixtureEditText(context).apply {
                id = idValue
                hint = label
                contentDescription = description
                setText(value)
                setTextColor(0xFFF3F4FF.toInt())
                setHintTextColor(0xFF737B94.toInt())
                textSize = 15f
                setPadding(16.dp(context), 12.dp(context), 16.dp(context), 12.dp(context))
                background = roundedBackground(0xFF141827.toInt(), 14.dp(context).toFloat())
                inputType =
                    if (multiline) {
                        InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
                    } else {
                        InputType.TYPE_CLASS_TEXT
                    }
                isSingleLine = !multiline
                minLines = if (multiline) 4 else 1
                gravity = if (multiline) Gravity.TOP or Gravity.START else Gravity.CENTER_VERTICAL
                addTextChangedListener(
                    object : TextWatcher {
                        override fun beforeTextChanged(
                            text: CharSequence?,
                            start: Int,
                            count: Int,
                            after: Int,
                        ) = Unit

                        override fun onTextChanged(
                            text: CharSequence?,
                            start: Int,
                            before: Int,
                            count: Int,
                        ) {
                            if (!syncingFromCompose) {
                                currentOnValueChanged(text.toString())
                            }
                        }

                        override fun afterTextChanged(text: Editable?) = Unit
                    },
                )
            }
        },
        update = { input ->
            if (input.text.toString() != value) {
                input.syncingFromCompose = true
                try {
                    input.setText(value)
                    input.setSelection(value.length)
                } finally {
                    input.syncingFromCompose = false
                }
            }
        },
    )
}

private class FixtureEditText(
    context: Context,
) : EditText(context) {
    var syncingFromCompose: Boolean = false
}

/**
 * Deliberately keeps a real platform View subtree inside the Compose shell.
 * ShadowDroid uses it to exercise Android View IDs, disabled state, clickable
 * ancestor resolution, ambiguous text, and true no-op delivery.
 */
@Composable
fun PlatformSelectorFixtures(
    counter: Int,
    onCounterChanged: (Int) -> Unit,
    onStatus: (String) -> Unit,
    onStartUnstableUpdates: () -> Unit,
    modifier: Modifier = Modifier,
) {
    AndroidView(
        modifier = modifier.fillMaxWidth(),
        factory = { context ->
            LinearLayout(context).apply {
                orientation = LinearLayout.VERTICAL

                addView(
                    fixtureLabel(context, R.id.counter_value, "Counter: $counter", "Counter value"),
                    fullWidth(context),
                )
                addView(
                    fixtureButton(
                        context,
                        R.id.counter_button,
                        "Increment counter",
                        "Increment counter button",
                    ) {
                        onCounterChanged(counter + 1)
                        onStatus("Counter incremented to ${counter + 1}")
                    },
                    fullWidth(context),
                )

                addView(
                    LinearLayout(context).apply {
                        orientation = LinearLayout.HORIZONTAL
                        addView(
                            fixtureButton(
                                context,
                                R.id.duplicate_one_button,
                                "Duplicate action",
                                "Duplicate action first",
                            ) { onStatus("First duplicate action tapped") },
                            weighted(context),
                        )
                        addView(
                            fixtureButton(
                                context,
                                R.id.duplicate_two_button,
                                "Duplicate action",
                                "Duplicate action second",
                            ) { onStatus("Second duplicate action tapped") },
                            weighted(context),
                        )
                    },
                    fullWidth(context),
                )

                addView(
                    fixtureButton(
                        context,
                        R.id.disabled_button,
                        "Disabled target",
                        "Disabled target button",
                    ) { onStatus("This should not run") }.apply {
                        isEnabled = false
                    },
                    fullWidth(context),
                )

                addView(
                    fixtureCard(
                        context,
                        idValue = R.id.nested_card,
                        description = "Nested clickable card",
                        onClick = { onStatus("Nested clickable ancestor activated") },
                    ).apply {
                        addView(
                            fixtureLabel(
                                context,
                                R.id.nested_card_label,
                                "Nested child action",
                                "Nested non-clickable child",
                            ),
                            fullWidth(context),
                        )
                    },
                    fullWidth(context),
                )

                addView(
                    fixtureCard(
                        context,
                        idValue = R.id.nested_outer_card,
                        description = "Outer clickable ancestor",
                        onClick = { onStatus("Outer clickable ancestor activated") },
                    ).apply {
                        addView(
                            fixtureCard(
                                context,
                                idValue = R.id.nested_inner_card,
                                description = "Inner clickable ancestor",
                                onClick = { onStatus("Nearest clickable ancestor activated") },
                            ).apply {
                                addView(
                                    fixtureLabel(
                                        context,
                                        R.id.nested_inner_label,
                                        "Nearest ancestor action",
                                        "Child with multiple clickable ancestors",
                                    ),
                                    fullWidth(context),
                                )
                            },
                            fullWidth(context),
                        )
                    },
                    fullWidth(context),
                )

                addView(
                    fixtureCard(
                        context,
                        idValue = R.id.disabled_card,
                        description = "Disabled clickable card",
                        onClick = { onStatus("Disabled card should not run") },
                    ).apply {
                        isEnabled = false
                        addView(
                            fixtureLabel(
                                context,
                                R.id.disabled_card_label,
                                "Disabled child action",
                                "Child inside disabled card",
                            ),
                            fullWidth(context),
                        )
                    },
                    fullWidth(context),
                )

                addView(
                    fixtureButton(
                        context,
                        R.id.noop_button,
                        "No-op action",
                        "Valid action without screen change",
                    ) {
                        // Intentionally no mutation. Delivery and observable
                        // outcome must remain separate for `ui tap --observe`.
                    },
                    fullWidth(context),
                )
                addView(
                    fixtureButton(
                        context,
                        R.id.unstable_updates_button,
                        "Start unstable updates",
                        "Start unstable accessibility updates",
                        onStartUnstableUpdates,
                    ),
                    fullWidth(context),
                )
            }
        },
        update = { root ->
            root.findViewById<TextView>(R.id.counter_value)?.text = "Counter: $counter"
            root.findViewById<Button>(R.id.counter_button)?.setOnClickListener {
                onCounterChanged(counter + 1)
                onStatus("Counter incremented to ${counter + 1}")
            }
        },
    )
}

/**
 * Native range controls remain native so the fixture keeps both platform
 * AccessibilityNodeInfo range metadata and Compose range semantics.
 */
@Composable
fun PlatformRangeFixtures(
    onStatus: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    AndroidView(
        modifier = modifier.fillMaxWidth(),
        factory = { context ->
            LinearLayout(context).apply {
                orientation = LinearLayout.VERTICAL

                addRange(
                    context,
                    title = "PLATFORM · CONTINUOUS",
                    seekBar =
                        fixtureSeekBar(
                            context,
                            R.id.platform_continuous_slider,
                            "Platform continuous slider",
                            maxValue = 1_000,
                            initialValue = 380,
                            onStatus = onStatus,
                        ),
                )
                addRange(
                    context,
                    title = "PLATFORM · DISCRETE",
                    seekBar =
                        fixtureSeekBar(
                            context,
                            R.id.platform_discrete_slider,
                            "Platform discrete slider",
                            maxValue = 4,
                            initialValue = 2,
                            onStatus = onStatus,
                        ).apply { keyProgressIncrement = 1 },
                )
                addRange(
                    context,
                    title = "PLATFORM · DISABLED",
                    seekBar =
                        fixtureSeekBar(
                            context,
                            R.id.platform_disabled_slider,
                            "Platform disabled slider",
                            maxValue = 100,
                            initialValue = 50,
                            onStatus = onStatus,
                        ).apply { isEnabled = false },
                )
                addRange(
                    context,
                    title = "PLATFORM · RTL",
                    seekBar =
                        fixtureSeekBar(
                            context,
                            R.id.platform_rtl_slider,
                            "Platform RTL slider",
                            maxValue = 100,
                            initialValue = 25,
                            onStatus = onStatus,
                        ).apply { layoutDirection = View.LAYOUT_DIRECTION_RTL },
                )

                addView(
                    fixtureEyebrow(context, "GESTURE-ONLY · NO RANGE SEMANTICS"),
                    fullWidth(context),
                )
                addView(
                    CoordinateOnlyRangeView(context) { percent ->
                        onStatus("Coordinate-only signal scrubber changed to $percent%")
                    }.apply {
                        id = R.id.coordinate_only_range
                        contentDescription = "Coordinate-only range surrogate"
                    },
                    LinearLayout.LayoutParams(
                        LinearLayout.LayoutParams.MATCH_PARENT,
                        68.dp(context),
                    ).apply {
                        topMargin = 4.dp(context)
                        bottomMargin = 8.dp(context)
                    },
                )
            }
        },
    )
}

private fun LinearLayout.addRange(
    context: Context,
    title: String,
    seekBar: SeekBar,
) {
    addView(fixtureEyebrow(context, title), fullWidth(context))
    addView(seekBar, fullWidth(context).apply { bottomMargin = 6.dp(context) })
}

private fun fixtureSeekBar(
    context: Context,
    idValue: Int,
    description: String,
    maxValue: Int,
    initialValue: Int,
    onStatus: (String) -> Unit,
): SeekBar =
    SeekBar(context).apply {
        id = idValue
        contentDescription = description
        max = maxValue
        progress = initialValue
        progressTintList = ColorStateList.valueOf(0xFF9B8CFF.toInt())
        thumbTintList = ColorStateList.valueOf(0xFFB8ADFF.toInt())
        setOnSeekBarChangeListener(
            object : SeekBar.OnSeekBarChangeListener {
                override fun onProgressChanged(
                    seekBar: SeekBar?,
                    progress: Int,
                    fromUser: Boolean,
                ) {
                    if (fromUser) onStatus("$description changed to $progress")
                }

                override fun onStartTrackingTouch(seekBar: SeekBar?) = Unit

                override fun onStopTrackingTouch(seekBar: SeekBar?) = Unit
            },
        )
    }

private fun fixtureCard(
    context: Context,
    idValue: Int,
    description: String,
    onClick: () -> Unit,
): LinearLayout =
    LinearLayout(context).apply {
        id = idValue
        orientation = LinearLayout.VERTICAL
        isClickable = true
        isFocusable = true
        contentDescription = description
        setPadding(14.dp(context), 12.dp(context), 14.dp(context), 12.dp(context))
        background = roundedBackground(0xFF1A2032.toInt(), 14.dp(context).toFloat())
        setOnClickListener { onClick() }
    }

private fun fixtureLabel(
    context: Context,
    idValue: Int,
    value: String,
    description: String,
): TextView =
    TextView(context).apply {
        id = idValue
        text = value
        contentDescription = description
        setTextColor(0xFFF3F4FF.toInt())
        textSize = 15f
        gravity = Gravity.CENTER_VERTICAL
        setPadding(4.dp(context), 8.dp(context), 4.dp(context), 8.dp(context))
    }

private fun fixtureEyebrow(context: Context, value: String): TextView =
    TextView(context).apply {
        text = value
        setTextColor(0xFFA9AFC3.toInt())
        textSize = 10f
        typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)
        letterSpacing = 0.08f
        setPadding(4.dp(context), 12.dp(context), 4.dp(context), 0)
    }

private fun fixtureButton(
    context: Context,
    idValue: Int,
    label: String,
    description: String,
    onClick: () -> Unit,
): Button =
    Button(context).apply {
        id = idValue
        text = label
        contentDescription = description
        isAllCaps = false
        setTextColor(0xFFF3F4FF.toInt())
        textSize = 14f
        typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        backgroundTintList =
            ColorStateList(
                arrayOf(
                    intArrayOf(-android.R.attr.state_enabled),
                    intArrayOf(),
                ),
                intArrayOf(
                    0xFF222638.toInt(),
                    0xFF30265B.toInt(),
                ),
            )
        setOnClickListener { onClick() }
    }

private fun roundedBackground(color: Int, radius: Float): GradientDrawable =
    GradientDrawable().apply {
        setColor(color)
        cornerRadius = radius
        setStroke(1, 0xFF343B52.toInt())
    }

private fun fullWidth(context: Context): LinearLayout.LayoutParams =
    LinearLayout.LayoutParams(
        LinearLayout.LayoutParams.MATCH_PARENT,
        LinearLayout.LayoutParams.WRAP_CONTENT,
    ).apply {
        topMargin = 6.dp(context)
    }

private fun weighted(context: Context): LinearLayout.LayoutParams =
    LinearLayout.LayoutParams(
        0,
        LinearLayout.LayoutParams.WRAP_CONTENT,
        1f,
    ).apply {
        marginStart = 3.dp(context)
        marginEnd = 3.dp(context)
    }

private fun Int.dp(context: Context): Int =
    (this * context.resources.displayMetrics.density).roundToInt()

private class CoordinateOnlyRangeView(
    context: Context,
    private val onChanged: (Int) -> Unit,
) : View(context) {
    private val trackPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = 0xFF343B52.toInt()
            strokeWidth = 8.dp(context).toFloat()
            strokeCap = Paint.Cap.ROUND
        }
    private val activePaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = 0xFF55D6E8.toInt()
            strokeWidth = 8.dp(context).toFloat()
            strokeCap = Paint.Cap.ROUND
        }
    private val thumbPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = 0xFFF3F4FF.toInt()
        }
    private val labelPaint =
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = 0xFFA9AFC3.toInt()
            textSize = 12.dp(context).toFloat()
            typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)
        }
    private var fraction = 0.63f

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        val start = 18.dp(context).toFloat()
        val end = width - start
        val y = height * 0.58f
        val thumbX = start + (end - start) * fraction
        canvas.drawText("SIGNAL SCRUBBER", start, 16.dp(context).toFloat(), labelPaint)
        canvas.drawLine(start, y, end, y, trackPaint)
        canvas.drawLine(start, y, thumbX, y, activePaint)
        canvas.drawCircle(thumbX, y, 9.dp(context).toFloat(), thumbPaint)
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (event.action != MotionEvent.ACTION_DOWN && event.action != MotionEvent.ACTION_MOVE) {
            return event.action == MotionEvent.ACTION_UP
        }
        val inset = 18.dp(context).toFloat()
        fraction = ((event.x - inset) / (width - inset * 2f)).coerceIn(0f, 1f)
        invalidate()
        onChanged((fraction * 100).roundToInt())
        return true
    }
}
