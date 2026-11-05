//! Demo -> video with the engine's own recorder (`CMovieRecorder`, engine2):
//! `playdemo <dem> --startmovie <name> webm framerate 30 no_unique_dir` -> `csgo/<name>.webm`
//! (AV1 + Vorbis, CPU AOM encoder under Wine) [proven in the Wine tools runtime, 2026-10-10].
//! Notes: ../idabins/worknotes/movie-scratch.md (option list, output paths, "Finishing movie" line).
//! Linux client: no valve_webm/wmf/avi .so in the Linux bin dir -> frame dumps (png) are the path.
//!
//! * [`MovieSpec`] builds and validates the `playdemo ... --startmovie` line.
//! * [`MovieSpec::suite`] renders the demo-to-video suite (the POC's `suites/movie_demo.txt`) through
//!   the testscript suite model, so the trailer, checkpoint and `mark` rules are the same as every
//!   other suite.
//! * [`judge`] checks what the recorder left behind (console.log lines, the file or frames, ffprobe).
//! * The tools live in `tools/video.rs`; `demo_to_video` runs the suite through `run_testscript`.

pub mod judge;

use crate::testscript::Suite;

pub const DEFAULT_FRAMERATE: u32 = 30;
/// Longest demo or movie name (it becomes a file stem and part of a suite name).
pub const MAX_NAME: usize = 64;

/// A demo or movie name: starts with an ASCII alphanumeric, then `[A-Za-z0-9_-]`, at most
/// [`MAX_NAME`] characters. Such a name cannot carry `;`, spaces, `//` or a path into a console line
/// or a file path.
pub fn valid_movie_name(s: &str) -> bool {
    s.len() <= MAX_NAME
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
pub const MAX_FRAMERATE: u32 = 240;
pub const MAX_DIMENSION: u32 = 8192;
/// movie-scratch.md: `bitrate n` takes 1..500 (kbps << 10 for the encoder).
pub const MAX_BITRATE: u32 = 500;

/// Output formats of `startmovie`. `webm` is the engine's encoder path; the others write frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovieFormat {
    Webm,
    Png,
    Tga,
    Jpg,
}

impl MovieFormat {
    pub fn parse(s: &str) -> Result<MovieFormat, String> {
        match s {
            "webm" => Ok(MovieFormat::Webm),
            "png" => Ok(MovieFormat::Png),
            "tga" => Ok(MovieFormat::Tga),
            "jpg" => Ok(MovieFormat::Jpg),
            other => Err(format!(
                "format {other:?} is not one of webm, png, tga, jpg"
            )),
        }
    }

    /// The `startmovie` keyword and the file extension (frames: `<name>NNNNNNNN.<ext>`).
    pub fn as_str(self) -> &'static str {
        match self {
            MovieFormat::Webm => "webm",
            MovieFormat::Png => "png",
            MovieFormat::Tga => "tga",
            MovieFormat::Jpg => "jpg",
        }
    }
}

/// One `playdemo` recording. Build with [`MovieSpec::new`], set options, then [`MovieSpec::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieSpec {
    /// demo file stem under `csgo/` (`vtest_src` for `csgo/vtest_src.dem`)
    pub demo: String,
    /// movie file stem (`vtest_movie` -> `csgo/vtest_movie.webm`, or frames `vtest_movie00000000.png`)
    pub name: String,
    pub format: MovieFormat,
    pub framerate: u32,
    /// both or neither (an odd size only makes the encoder fail)
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// webm only
    pub bitrate: Option<u32>,
}

impl MovieSpec {
    pub fn new(demo: impl Into<String>, name: impl Into<String>, format: MovieFormat) -> Self {
        MovieSpec {
            demo: demo.into(),
            name: name.into(),
            format,
            framerate: DEFAULT_FRAMERATE,
            width: None,
            height: None,
            bitrate: None,
        }
    }

    pub fn framerate(mut self, fps: u32) -> Self {
        self.framerate = fps;
        self
    }

    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = Some(width);
        self.height = Some(height);
        self
    }

    pub fn bitrate(mut self, kbps: u32) -> Self {
        self.bitrate = Some(kbps);
        self
    }

    /// Refuses what the engine would reject or misread. Names use the suite charset
    /// `[A-Za-z0-9_-]+` so they cannot carry `;`, spaces or `//` into a console line.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_movie_name(&self.demo) {
            return Err(format!(
                "demo {:?} must be a file stem: alphanumeric first, then [A-Za-z0-9_-], at most {MAX_NAME} characters",
                self.demo
            ));
        }
        if !valid_movie_name(&self.name) {
            return Err(format!(
                "name {:?} must be a movie stem: alphanumeric first, then [A-Za-z0-9_-], at most {MAX_NAME} characters",
                self.name
            ));
        }
        if !(1..=MAX_FRAMERATE).contains(&self.framerate) {
            return Err(format!("framerate must be 1..={MAX_FRAMERATE}"));
        }
        match (self.width, self.height) {
            (None, None) => {}
            (Some(w), Some(h)) => {
                if !(1..=MAX_DIMENSION).contains(&w) || !(1..=MAX_DIMENSION).contains(&h) {
                    return Err(format!("width and height must be 1..={MAX_DIMENSION}"));
                }
            }
            _ => return Err("width and height must be given together".into()),
        }
        if let Some(b) = self.bitrate {
            if self.format != MovieFormat::Webm {
                return Err("bitrate applies to webm only".into());
            }
            if !(1..=MAX_BITRATE).contains(&b) {
                return Err(format!("bitrate must be 1..={MAX_BITRATE}"));
            }
        }
        Ok(())
    }

    /// The argument list after `playdemo`: `<demo> --startmovie <name> <format> framerate N
    /// [bitrate B] [width W height H] no_unique_dir`. `no_unique_dir` keeps the output in `csgo/`
    /// (no `movie/<timestamp>/` folder), so the judge finds it by name.
    pub fn playdemo_args(&self) -> String {
        let mut s = format!(
            "{} --startmovie {} {} framerate {}",
            self.demo,
            self.name,
            self.format.as_str(),
            self.framerate
        );
        if let Some(b) = self.bitrate {
            s.push_str(&format!(" bitrate {b}"));
        }
        if let (Some(w), Some(h)) = (self.width, self.height) {
            s.push_str(&format!(" width {w} height {h}"));
        }
        s.push_str(" no_unique_dir");
        s
    }

    /// The suite body (commands only; `Suite::render` appends the trailer). Mirrors the POC
    /// `worknotes/vtest/suites/movie_demo.txt`: the demo plays, the recorder ends itself on
    /// `DemoPlaybackFinished`, then a short drain before the final checkpoint and `quit`.
    pub fn suite_body(&self) -> String {
        [
            "testscript_debug 1".to_string(),
            "r_always_render_all_windows 1".to_string(),
            "Test_Run mark VTEST_BEGIN".to_string(),
            format!("playdemo {}", self.playdemo_args()),
            "Test_WaitForCheckPoint DemoPlaybackFinished".to_string(),
            "Test_Run mark VTEST_DEMO_DONE".to_string(),
            "// let the encoder drain and the muxer finalize".to_string(),
            "Test_Wait 5".to_string(),
            "Test_Run mark VTEST_DONE".to_string(),
        ]
        .join("\n")
            + "\n"
    }

    /// The staged suite: `movie_<name>_<stamp>` (a file stem under `vtest/`). The stamp (ms since
    /// the epoch at the call) makes every staging a new file, so a suite name is never rewritten
    /// under a session that may still be reading it.
    pub fn suite(&self, stamp: u128) -> Suite {
        Suite::inline(&format!("movie_{}_{stamp}", self.name), &self.suite_body())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POC_SUITE: &str = include_str!("../../fixtures/video/movie_demo.poc.txt");

    /// The POC file's command lines (comments and blank lines dropped).
    fn commands(text: &str) -> Vec<&str> {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .collect()
    }

    fn poc_spec() -> MovieSpec {
        MovieSpec::new("vtest_src", "vtest_movie", MovieFormat::Webm)
    }

    #[test]
    fn playdemo_line_golden_matches_the_poc_command() {
        assert_eq!(
            poc_spec().playdemo_args(),
            "vtest_src --startmovie vtest_movie webm framerate 30 no_unique_dir"
        );
    }

    #[test]
    fn option_goldens_keep_order_and_optional_parts() {
        let s = poc_spec().framerate(60).bitrate(20).size(1280, 720);
        s.validate().unwrap();
        assert_eq!(
            s.playdemo_args(),
            "vtest_src --startmovie vtest_movie webm framerate 60 bitrate 20 width 1280 height 720 no_unique_dir"
        );
        let png = MovieSpec::new("d", "frames", MovieFormat::Png).size(640, 360);
        png.validate().unwrap();
        assert_eq!(
            png.playdemo_args(),
            "d --startmovie frames png framerate 30 width 640 height 360 no_unique_dir"
        );
    }

    #[test]
    fn format_parse_accepts_only_the_four_formats() {
        for (s, f) in [
            ("webm", MovieFormat::Webm),
            ("png", MovieFormat::Png),
            ("tga", MovieFormat::Tga),
            ("jpg", MovieFormat::Jpg),
        ] {
            assert_eq!(MovieFormat::parse(s).unwrap(), f);
            assert_eq!(f.as_str(), s);
        }
        for bad in ["mp4", "avi", "jpeg", "WEBM", "", "png;quit"] {
            assert!(MovieFormat::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validation_refuses_bad_names_and_options() {
        let bad_names = [
            MovieSpec::new("", "m", MovieFormat::Webm),
            MovieSpec::new("d", "m;quit", MovieFormat::Webm),
            MovieSpec::new("d", "a b", MovieFormat::Webm),
            MovieSpec::new("d", "../x", MovieFormat::Webm),
            MovieSpec::new("d/x", "m", MovieFormat::Webm),
            MovieSpec::new("d", "m.webm", MovieFormat::Webm),
            MovieSpec::new("d", "-leading", MovieFormat::Webm),
            MovieSpec::new("d", "_leading", MovieFormat::Webm),
            MovieSpec::new("d", "a".repeat(65), MovieFormat::Webm),
        ];
        MovieSpec::new("d", "a".repeat(64), MovieFormat::Webm)
            .validate()
            .expect("64 characters is the longest name");
        for s in &bad_names {
            assert!(s.validate().is_err(), "{s:?}");
        }
        assert!(poc_spec().framerate(0).validate().is_err());
        assert!(poc_spec().framerate(241).validate().is_err());
        assert!(poc_spec().bitrate(0).validate().is_err());
        assert!(poc_spec().bitrate(501).validate().is_err());
        assert!(MovieSpec::new("d", "m", MovieFormat::Png)
            .bitrate(10)
            .validate()
            .is_err());
        let mut half = poc_spec();
        half.width = Some(640);
        assert!(half.validate().is_err(), "width without height");
        assert!(poc_spec().size(0, 720).validate().is_err());
        assert!(poc_spec().size(8193, 720).validate().is_err());
    }

    #[test]
    fn suite_commands_match_the_poc_movie_demo_byte_for_byte() {
        let rendered = poc_spec().suite(1).render().unwrap();
        assert_eq!(commands(&rendered), commands(POC_SUITE));
    }

    #[test]
    fn suite_golden_is_the_full_rendered_script() {
        let rendered = poc_spec().suite(1).render().unwrap();
        assert_eq!(
            rendered,
            "testscript_debug 1\nr_always_render_all_windows 1\nTest_Run mark VTEST_BEGIN\n\
             playdemo vtest_src --startmovie vtest_movie webm framerate 30 no_unique_dir\n\
             Test_WaitForCheckPoint DemoPlaybackFinished\nTest_Run mark VTEST_DEMO_DONE\n\
             // let the encoder drain and the muxer finalize\nTest_Wait 5\nTest_Run mark VTEST_DONE\n\
             quit\nTest_Wait 30\nTest_ExitProcess 3\n"
        );
        assert_eq!(poc_spec().suite(1234).name, "movie_vtest_movie_1234");
    }
}
