package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import android.graphics.Color
import android.os.Bundle
import android.view.Gravity
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView

/** Deterministic native focus surface shared by phone and Android TV E2E. */
class FocusFixtureActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val density = resources.displayMetrics.density
        fun dp(value: Int): Int = (value * density).toInt()

        val status =
            TextView(this).apply {
                id = R.id.focus_status
                text = "Focus origin ready"
                textSize = 22f
                setTextColor(Color.WHITE)
                gravity = Gravity.CENTER
            }
        val origin =
            Button(this).apply {
                id = R.id.focus_origin_button
                text = "Focus origin"
                contentDescription = "Focus origin button"
                isFocusable = true
                isFocusableInTouchMode = true
                nextFocusRightId = R.id.focus_target_button
            }
        val target =
            Button(this).apply {
                id = R.id.focus_target_button
                text = "Focus target"
                contentDescription = "Focus target button"
                isFocusable = true
                nextFocusLeftId = R.id.focus_origin_button
                setOnClickListener { status.text = "Focus target activated" }
            }

        val controls =
            LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER
                addView(origin, weighted())
                addView(target, weighted())
            }
        val root =
            LinearLayout(this).apply {
                id = R.id.focus_fixture_root
                orientation = LinearLayout.VERTICAL
                gravity = Gravity.CENTER
                setPadding(dp(32), dp(32), dp(32), dp(32))
                setBackgroundColor(Color.rgb(10, 13, 24))
                addView(
                    status,
                    LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        dp(96),
                    ),
                )
                addView(
                    controls,
                    LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        dp(96),
                    ),
                )
            }
        setContentView(root)
        root.post { origin.requestFocus() }
    }

    private fun weighted(): LinearLayout.LayoutParams =
        LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1f).apply {
            marginStart = 12
            marginEnd = 12
        }
}
