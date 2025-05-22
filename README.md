# macOS App Usage Tracker

A simple command-line tool that tracks your application usage on macOS, including browser tabs and time spent on each application. The tool runs in the background and records your activity, saving the data in an easy-to-analyze CSV format.

## Features

- Tracks active applications and their usage time
- Monitors browser tabs (Chrome, Safari, Brave, Edge)
- Categorizes applications automatically
- Saves data in CSV format for easy analysis
- Graceful shutdown with Ctrl+C
- Persistent storage across sessions
- Desktop-friendly output

## Requirements

- macOS (tested on macOS 10.15 and later)
- Rust toolchain (for building from source)

## Installation

### From Source

1. Clone the repository:
```bash
git clone https://github.com/yourusername/app-tracker-demo.git
cd app-tracker-demo
```

2. Build the release version:
```bash
cargo build --release
```

3. The executable will be in `target/release/app_tracker_macos`

### Pre-built Binary

Download the latest release from the releases page and run it directly.

## Usage

1. Run the application:
```bash
./app_tracker_macos
```

2. The tracker will start running in the background. You'll see:
   - A startup message
   - Notifications when switching between applications
   - Current tracking status

3. To stop tracking and view the summary:
   - Press `Ctrl+C`
   - The program will save the current session
   - Display a usage summary
   - Save the data to `usage_stats.csv` on your Desktop

## Output Format

The program creates a `usage_stats.csv` file on your Desktop with the following columns:

- Start Time: When the application/tab became active
- End Time: When the application/tab was switched away
- Duration (seconds): Time spent on the application/tab
- App Name: Name of the application
- Bundle ID: macOS bundle identifier
- Category: Application category (Browser, Terminal, Email, etc.)
- URL: Current URL (for browsers only)

## Categories

Applications are automatically categorized into:
- Browser (Chrome, Safari, Brave, Edge)
- Terminal (Terminal, iTerm2)
- Email (Mail, Outlook)
- Communication (Slack, Teams)
- Productivity (Notes, TextEdit)
- Uncategorized (other applications)

## Building from Source

1. Install Rust:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

2. Clone and build:
```bash
git clone https://github.com/yourusername/app-tracker-demo.git
cd app-tracker-demo
cargo build --release
```

## Dependencies

- chrono: For timestamp handling
- serde: For data serialization
- csv: For CSV file handling
- ctrlc: For graceful shutdown handling

## Privacy

- The tracker only monitors active applications and browser tabs
- All data is stored locally in the CSV file
- No data is sent to external servers
- You can delete the CSV file at any time to clear the history

## Limitations

- Requires permission to monitor applications (first run)
- Browser URL tracking works with:
  - Google Chrome
  - Safari
  - Brave Browser
  - Microsoft Edge
- Some applications may not report their bundle ID correctly
- System sleep/wake cycles may affect timing accuracy

## Contributing

Feel free to submit issues and enhancement requests!

## License

MIT License - feel free to use this in your own projects!

## Acknowledgments

- Uses AppleScript for macOS integration
- Inspired by various time-tracking tools
- Built with Rust for performance and reliability