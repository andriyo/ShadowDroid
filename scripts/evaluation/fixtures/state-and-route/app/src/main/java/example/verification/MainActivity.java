package example.verification;

import android.app.Activity;
import android.content.Intent;
import android.content.res.Configuration;
import android.os.Bundle;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.TextView;

public class MainActivity extends Activity {
    private EditText draft;

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        TextView title = new TextView(this);
        title.setId(R.id.title);
        title.setText("Draft editor");
        root.addView(title);
        draft = new EditText(this);
        draft.setId(R.id.draft);
        draft.setHint("Your draft");
        draft.setSaveEnabled(false);
        root.addView(draft);
        TextView theme = new TextView(this);
        theme.setId(R.id.theme);
        theme.setText("Light");
        root.addView(theme);
        Button next = new Button(this);
        next.setId(R.id.next);
        next.setText("Details");
        next.setOnClickListener(view -> { /* Route not implemented yet. */ });
        root.addView(next);
        setContentView(root);
    }

    public static class DetailActivity extends Activity {
        @Override public void onCreate(Bundle state) {
            super.onCreate(state);
            TextView detail = new TextView(this);
            detail.setId(R.id.detail);
            detail.setText("Details ready");
            setContentView(detail);
        }
    }
}
