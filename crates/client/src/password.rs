//! Password field like mstsc / the Windows sign-in screen: no input method (no Hangul composition,
//! no composition window showing the typed characters), and Hangul that still arrives (a Korean
//! keyboard layout without its IME) is read as the 2-set (Dubeolsik) keys that typed it, i.e.
//! the password is what the keys say in the English layout.
//!
//! iced asks for the IME with `Purpose::Secure` on secure text inputs, but winit ignores the
//! purpose on macOS and Windows, so the wrapper turns the request into `InputMethod::Disabled`.
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{self, Operation, Tree, Widget};
use iced::advanced::{Clipboard, InputMethod, Shell, overlay};
use iced::{Element, Event, Length, Rectangle, Size, Vector, mouse};

/// Wraps a secure text input so it never enables the input method.
pub struct NoIme<'a, Message, Theme, Renderer>(Element<'a, Message, Theme, Renderer>);

pub fn no_ime<'a, Message, Theme, Renderer>(e: impl Into<Element<'a, Message, Theme, Renderer>>) -> NoIme<'a, Message, Theme, Renderer> {
    NoIme(e.into())
}

impl<Message, Theme, Renderer: renderer::Renderer> Widget<Message, Theme, Renderer> for NoIme<'_, Message, Theme, Renderer> {
    fn size(&self) -> Size<Length> {
        self.0.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.0.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.0.as_widget_mut().layout(tree, renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.0.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport)
    }

    fn tag(&self) -> widget::tree::Tag {
        self.0.as_widget().tag()
    }

    fn state(&self) -> widget::tree::State {
        self.0.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.0.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.0.as_widget().diff(tree)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.0.as_widget_mut().operate(tree, layout, renderer, operation)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let before = matches!(shell.input_method(), InputMethod::Enabled { .. });
        self.0.as_widget_mut().update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
        // only this (focused) field asked for it: take it back
        if !before && matches!(shell.input_method(), InputMethod::Enabled { .. }) {
            *shell.input_method_mut() = InputMethod::Disabled;
        }
    }

    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle, renderer: &Renderer) -> mouse::Interaction {
        self.0.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.0.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer: renderer::Renderer + 'a> From<NoIme<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer> {
    fn from(w: NoIme<'a, Message, Theme, Renderer>) -> Self {
        Element::new(w)
    }
}

/// Hangul compatibility jamo -> 2-set keys.
fn jamo_keys(c: char) -> Option<&'static str> {
    Some(match c {
        'ㅂ' => "q",
        'ㅈ' => "w",
        'ㄷ' => "e",
        'ㄱ' => "r",
        'ㅅ' => "t",
        'ㅛ' => "y",
        'ㅕ' => "u",
        'ㅑ' => "i",
        'ㅐ' => "o",
        'ㅔ' => "p",
        'ㅁ' => "a",
        'ㄴ' => "s",
        'ㅇ' => "d",
        'ㄹ' => "f",
        'ㅎ' => "g",
        'ㅗ' => "h",
        'ㅓ' => "j",
        'ㅏ' => "k",
        'ㅣ' => "l",
        'ㅋ' => "z",
        'ㅌ' => "x",
        'ㅊ' => "c",
        'ㅍ' => "v",
        'ㅠ' => "b",
        'ㅜ' => "n",
        'ㅡ' => "m",
        'ㅃ' => "Q",
        'ㅉ' => "W",
        'ㄸ' => "E",
        'ㄲ' => "R",
        'ㅆ' => "T",
        'ㅒ' => "O",
        'ㅖ' => "P",
        // compound vowels / final consonant clusters: two keys
        'ㅘ' => "hk",
        'ㅙ' => "ho",
        'ㅚ' => "hl",
        'ㅝ' => "nj",
        'ㅞ' => "np",
        'ㅟ' => "nl",
        'ㅢ' => "ml",
        'ㄳ' => "rt",
        'ㄵ' => "sw",
        'ㄶ' => "sg",
        'ㄺ' => "fr",
        'ㄻ' => "fa",
        'ㄼ' => "fq",
        'ㄽ' => "ft",
        'ㄾ' => "fx",
        'ㄿ' => "fv",
        'ㅀ' => "fg",
        'ㅄ' => "qt",
        _ => return None,
    })
}

const CHOSEONG: [char; 19] = ['ㄱ', 'ㄲ', 'ㄴ', 'ㄷ', 'ㄸ', 'ㄹ', 'ㅁ', 'ㅂ', 'ㅃ', 'ㅅ', 'ㅆ', 'ㅇ', 'ㅈ', 'ㅉ', 'ㅊ', 'ㅋ', 'ㅌ', 'ㅍ', 'ㅎ'];
const JUNGSEONG: [char; 21] = ['ㅏ', 'ㅐ', 'ㅑ', 'ㅒ', 'ㅓ', 'ㅔ', 'ㅕ', 'ㅖ', 'ㅗ', 'ㅘ', 'ㅙ', 'ㅚ', 'ㅛ', 'ㅜ', 'ㅝ', 'ㅞ', 'ㅟ', 'ㅠ', 'ㅡ', 'ㅢ', 'ㅣ'];
const JONGSEONG: [Option<char>; 28] = [
    None,
    Some('ㄱ'),
    Some('ㄲ'),
    Some('ㄳ'),
    Some('ㄴ'),
    Some('ㄵ'),
    Some('ㄶ'),
    Some('ㄷ'),
    Some('ㄹ'),
    Some('ㄺ'),
    Some('ㄻ'),
    Some('ㄼ'),
    Some('ㄽ'),
    Some('ㄾ'),
    Some('ㄿ'),
    Some('ㅀ'),
    Some('ㅁ'),
    Some('ㅂ'),
    Some('ㅄ'),
    Some('ㅅ'),
    Some('ㅆ'),
    Some('ㅇ'),
    Some('ㅈ'),
    Some('ㅊ'),
    Some('ㅋ'),
    Some('ㅌ'),
    Some('ㅍ'),
    Some('ㅎ'),
];

/// Hangul (jamo or precomposed syllables) -> the 2-set keys that type it; everything else unchanged.
pub fn latin_keys(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let code = c as u32;
        if (0xAC00..=0xD7A3).contains(&code) {
            let i = code - 0xAC00;
            let (cho, jung, jong) = ((i / 588) as usize, (i % 588 / 28) as usize, (i % 28) as usize);
            for j in [Some(CHOSEONG[cho]), Some(JUNGSEONG[jung]), JONGSEONG[jong]].into_iter().flatten() {
                out.push_str(jamo_keys(j).unwrap_or_default());
            }
        } else if let Some(k) = jamo_keys(c) {
            out.push_str(k);
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::latin_keys;

    #[test]
    fn hangul_becomes_the_keys_that_typed_it() {
        assert_eq!(latin_keys("ㅁㄴㅇㄹ"), "asdf");
        assert_eq!(latin_keys("한글"), "gksrmf");
        assert_eq!(latin_keys("ㅃ쌍"), "QTkd", "shifted keys");
        assert_eq!(latin_keys("괜찮"), "rhoscksg", "compound vowel and final cluster");
        assert_eq!(latin_keys("Pass1!"), "Pass1!", "everything else unchanged");
    }
}
