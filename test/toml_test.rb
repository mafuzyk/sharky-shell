# frozen_string_literal: true

require_relative "test_helper"
require "sharky/toml"

class TomlTest < Minitest::Test
  def test_parses_flat_strings_and_comments
    data = Sharky::Toml.parse(<<~TOML)
      # theme colors
      prompt = "1;31" # red
      path = "38;2;1;2;3"
    TOML

    assert_equal({ "prompt" => "1;31", "path" => "38;2;1;2;3" }, data)
  end

  def test_parses_styles_table
    data = Sharky::Toml.parse(<<~TOML)
      [styles]
      prompt = "31"
      error = "1;31"
    TOML

    assert_equal({ "styles" => { "prompt" => "31", "error" => "1;31" } }, data)
  end

  def test_parses_basic_string_escapes
    assert_equal "a\"b\nc", Sharky::Toml.parse('key = "a\\"b\\nc"')["key"]
  end

  def test_parses_literal_strings
    assert_equal 'a\b', Sharky::Toml.parse("key = 'a\\b'")["key"]
  end

  def test_hash_inside_string_is_not_a_comment
    assert_equal "a#b", Sharky::Toml.parse('key = "a#b"')["key"]
  end

  def test_rejects_missing_value
    assert_raises(Sharky::Toml::ParseError) { Sharky::Toml.parse("key =") }
  end

  def test_rejects_unterminated_string
    assert_raises(Sharky::Toml::ParseError) { Sharky::Toml.parse('key = "oops') }
  end

  def test_rejects_non_string_flat_values
    assert_raises(Sharky::Toml::ParseError) { Sharky::Toml.parse("key = 1") }
    assert_raises(Sharky::Toml::ParseError) { Sharky::Toml.parse("key = true") }
  end

  def test_reports_line_numbers
    error = assert_raises(Sharky::Toml::ParseError) { Sharky::Toml.parse("prompt = \"31\"\nbroken = 1\n") }
    assert_includes error.message, "line 2"
  end

  def test_parses_arrays_of_tables_in_order
    data = parse(<<~TOML)
      [[lines]]
      suffix = ""

      [[lines]]
      suffix = "> "

      [[segments]]
      line = 1
      text = "{os}"

      [[segments]]
      line = 2
      text = "{git}"
    TOML

    assert_equal [{ "suffix" => "" }, { "suffix" => "> " }], data["lines"]
    assert_equal [
      { "line" => 1, "text" => "{os}" },
      { "line" => 2, "text" => "{git}" }
    ], data["segments"]
  end

  def test_parses_nested_tables
    data = parse(<<~TOML)
      [prompt]
      separator = "fire"

      [prompt.colors]
      accent = "pink"
      enabled = true

      [prompt.colors.extra]
      depth = 2
    TOML

    assert_equal(
      {
        "prompt" => {
          "separator" => "fire",
          "colors" => {
            "accent" => "pink",
            "enabled" => true,
            "extra" => { "depth" => 2 }
          }
        }
      },
      data
    )
  end

  def test_parses_integers_and_booleans_inside_tables
    data = parse(<<~TOML)
      [prompt]
      count = 2
      negative = -3
      zero = 0
      enabled = true
      disabled = false
    TOML

    assert_equal(
      {
        "count" => 2,
        "negative" => -3,
        "zero" => 0,
        "enabled" => true,
        "disabled" => false
      },
      data["prompt"]
    )
  end

  def test_rejects_duplicate_key
    error = assert_raises(Sharky::Toml::ParseError) do
      parse("key = \"a\"\nkey = \"b\"\n")
    end
    assert_includes error.message, "duplicate key"
    assert_includes error.message, "line 2"
  end

  def test_rejects_duplicate_table
    error = assert_raises(Sharky::Toml::ParseError) do
      parse("[styles]\nprompt = \"31\"\n[styles]\nerror = \"31\"\n")
    end
    assert_includes error.message, "duplicate table"
    assert_includes error.message, "line 3"
  end

  def test_rejects_duplicate_key_inside_array_table
    error = assert_raises(Sharky::Toml::ParseError) do
      parse("[[lines]]\nsuffix = \"\"\nsuffix = \"> \"\n")
    end
    assert_includes error.message, "duplicate key"
    assert_includes error.message, "line 3"
  end

  def test_rejects_empty_table_name_parts
    assert_raises(Sharky::Toml::ParseError) { parse("[.]\nkey = \"value\"\n") }
    assert_raises(Sharky::Toml::ParseError) { parse("[prompt.]\nkey = \"value\"\n") }
    assert_raises(Sharky::Toml::ParseError) { parse("[prompt..colors]\nkey = \"value\"\n") }
  end

  def test_reports_path_and_line_for_duplicate_keys
    path = File.join(@tmp, "layout.toml")
    error = assert_raises(Sharky::Toml::ParseError) do
      parse("[[segments]]\nline = 1\nline = 2\n", path: path)
    end
    assert_includes error.message, "#{path}:line 3"
  end

  def test_parses_complete_layout_fixture
    data = parse(<<~TOML)
      [prompt]
      separator = "fire"
      leading = "square_fade"
      trailing = "point_fade"
      padding = 1

      [[lines]]
      suffix = ""

      [[lines]]
      suffix = "> "

      [[segments]]
      line = 1
      icon = ""
      text = "{os}"
      bg = "arch"
      fg = "ink"
      when = "always"

      [[segments]]
      line = 1
      icon = ""
      text = "{arch}"
      bg = "pink"
      fg = "ink"
      when = "always"

      [[segments]]
      line = 1
      icon = ""
      text = "{user}"
      bg = "white"
      fg = "ink"
      when = "always"

      [[segments]]
      line = 1
      icon = ""
      text = "{cwd}"
      bg = "white"
      fg = "ink"
      when = "always"

      [[segments]]
      line = 1
      icon = ""
      text = "{git}"
      bg = "pink"
      fg = "ink"
      when = "git"

      [[segments]]
      line = 1
      icon = ""
      text = "{duration}"
      bg = "blue"
      fg = "ink"
      when = "always"

      [[segments]]
      line = 2
      text = "🐰 🌸"
      bg = "pink"
      fg = "ink"
      when = "always"
    TOML

    assert_equal(
      {
        "separator" => "fire",
        "leading" => "square_fade",
        "trailing" => "point_fade",
        "padding" => 1
      },
      data["prompt"]
    )
    assert_equal [{ "suffix" => "" }, { "suffix" => "> " }], data["lines"]
    assert_equal 7, data["segments"].length
    assert_equal [1, 1, 1, 1, 1, 1, 2], data["segments"].map { |segment| segment["line"] }
    assert_equal ["{os}", "{arch}", "{user}", "{cwd}", "{git}", "{duration}"],
                 data["segments"].first(6).map { |segment| segment["text"] }
    assert_equal "🐰 🌸", data["segments"].last["text"]
  end

  private

  def parse(source, path: nil)
    Sharky::Toml.parse(source, path: path)
  end
end
