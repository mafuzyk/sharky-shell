# frozen_string_literal: true

require_relative "test_helper"

class StartupTest < Minitest::Test
  def test_requiring_sharky_defers_execution_and_interactive_code
    out, err, status, features = observe_startup("-I#{File.join(ROOT, 'lib')}", "-e", <<~RUBY)
      require "sharky"
      puts Sharky::VERSION
      abort "public API is unavailable" unless Sharky.respond_to?(:command)
    RUBY

    assert status.success?, err
    assert_equal "1.0.1\n", out
    assert_equal "", err
    assert_deferred features
  end

  def test_information_options_do_not_load_execution_or_interactive_code
    ["--version", "--help", "-h"].each do |option|
      out, err, status, features = observe_startup(BIN, option)

      assert status.success?, "#{option}: #{err}"
      assert_equal "", err
      assert_includes out, option == "--version" ? "sharky 1.0.1\n" : "Usage: sharky"
      assert_deferred features
      refute_includes features, "/json.rb"
      refute_includes features, "/socket.rb"
    end
  end

  def test_non_interactive_commands_do_not_load_terminal_helpers
    out, err, status, features = observe_startup(BIN, "--no-config", "-c", "true")

    assert status.success?, err
    assert_equal "", out
    assert_equal "", err
    %w[editor terminal prompt starship].each do |name|
      refute_includes features, "/sharky/#{name}.rb"
    end
    refute_includes features, "/socket.rb"
  end

  def test_all_public_constants_remain_available_in_reverse_load_order
    out, err, status, = observe_startup("-I#{File.join(ROOT, 'lib')}", "-e", <<~RUBY)
      require "sharky"
      names = %i[
        AST Arithmetic Builtins CLI Completion Config ControlFlow Editor Executor
        Expansion Functions Fuzzy Highlighter History Jobs Key Keys LexError Lexer
        LineBuffer Pager ParseError Parser Prompt PromptLayout PromptPresets Repl
        Result Runtime Session Shell Starship Terminal Theme Token VERSION Z
      ]
      abort "public constants are missing" unless (names - Sharky.constants(false)).empty?
      names.reverse_each { |name| Sharky.const_get(name, false) }
      puts "public constants loaded"
    RUBY

    assert status.success?, err
    assert_equal "public constants loaded\n", out
    assert_equal "", err
  end

  private

  def observe_startup(*argv)
    features_path = File.join(@tmp, "loaded-features")
    observer = File.join(@tmp, "observe.rb")
    File.write(observer, <<~RUBY)
      at_exit { File.write(#{features_path.dump}, $LOADED_FEATURES.join("\n")) }
    RUBY
    out, err, status = Open3.capture3(
      { "HOME" => @tmp }, RbConfig.ruby, "--disable-gems", "-r", observer, *argv
    )
    [out, err, status, File.read(features_path)]
  end

  def assert_deferred(features)
    %w[parser executor editor terminal prompt starship].each do |name|
      refute_includes features, "/sharky/#{name}.rb", "#{name} should load only when needed"
    end
  end
end
