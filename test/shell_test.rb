require_relative "test_helper"
require "sharky"
require "stringio"

class ShellTest < Minitest::Test
  def setup
    super
    @shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new, stdin: StringIO.new)
  end

  def test_runs_external_commands_and_status
    assert_equal 0, @shell.run("true")
    assert_equal 1, @shell.run("false")
    assert_equal 1, @shell.last_status
  end

  def test_cd_persists_and_pwd_prints
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)
    assert_equal 0, @shell.run("cd #{target}")
    assert_equal target, @shell.cwd
    assert_equal 0, @shell.run("pwd")
    assert_equal "#{target}\n", @shell.stdout.string
  end

  def test_external_commands_run_in_shell_cwd
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)
    assert_equal 0, @shell.run("cd #{target}")
    assert_equal 0, @shell.run("/usr/bin/pwd")
    assert_equal "#{target}\n", @shell.stdout.string
  end

  def test_export_and_unset
    assert_equal 0, @shell.run("export A=1")
    assert_equal "1", @shell.env["A"]
    assert_equal 0, @shell.run("unset A")
    assert_nil @shell.env["A"]
  end

  def test_assignment_only_persists
    assert_equal 0, @shell.run("B=2")
    assert_equal "2", @shell.env["B"]
  end

  def test_unknown_command_is_127
    assert_equal 127, @shell.run("definitely-not-a-command")
    assert_includes @shell.stderr.string, "command not found"
  end

  def test_exit_sets_flag_and_status
    assert_equal 7, @shell.run("exit 7")
    assert @shell.exit_requested?
    assert_equal 7, @shell.last_status
  end

  def test_exit_stops_following_commands
    assert_equal 0, @shell.run("exit 0; printf no")
    assert_equal "", @shell.stdout.string
  end

  def test_parameter_error_is_reported_without_crashing
    assert_equal 1, @shell.run("echo ${MISSING:?oops}")
    assert_equal "", @shell.stdout.string
    assert_match(/MISSING: oops/, @shell.stderr.string)
  end

  def test_arithmetic_error_is_reported_without_crashing
    assert_equal 1, @shell.run("echo $((1 / 0))")
    assert_match(/division by zero/, @shell.stderr.string)
  end

  def test_set_positional_parameters
    assert_equal 0, @shell.run("set -- a b")
    assert_equal %w[a b], @shell.positional
    assert_equal 0, @shell.run(%q{printf '%s|%s' "$1" "$#"})
    assert_equal "a|2", @shell.stdout.string
  end

  def test_mixed_and_or_recovers_after_a_skipped_command
    assert_equal 0, @shell.run('false && printf no || printf recovered')
    assert_equal "recovered", @shell.stdout.string
  end

  def test_mixed_or_and_runs_after_a_skipped_command
    assert_equal 0, @shell.run('true || printf no && printf yes')
    assert_equal "yes", @shell.stdout.string
  end

  def test_set_e_stops_after_failure
    assert_equal 1, @shell.run("set -e; false; printf no")
    assert_equal "", @shell.stdout.string
    assert @shell.exit_requested?
  end

  def test_errexit_does_not_exit_for_an_and_guard
    assert_equal 0, @shell.run('set -e; false && printf no; printf alive')
    assert_equal "alive", @shell.stdout.string
    refute @shell.exit_requested?
  end

  def test_errexit_does_not_exit_for_a_negated_command
    assert_equal 0, @shell.run('set -e; ! true; printf alive', compound: true)
    assert_equal "alive", @shell.stdout.string
    refute @shell.exit_requested?
  end

  def test_errexit_guard_context_reaches_group_body
    assert_equal 0, @shell.run('set -e; { false; printf body; } && printf ok', compound: true)
    assert_equal "bodyok", @shell.stdout.string
    refute @shell.exit_requested?
  end

  def test_errexit_guard_context_reaches_function_body
    source = 'work() { false; printf body; }; set -e; work && printf ok'
    assert_equal 0, @shell.run(source, compound: true)
    assert_equal "bodyok", @shell.stdout.string
    refute @shell.exit_requested?
  end

  def test_errexit_condition_context_reaches_nested_groups
    source = 'set -e; if { false; printf condition; }; then printf body; fi'
    assert_equal 0, @shell.run(source, compound: true)
    assert_equal "conditionbody", @shell.stdout.string
    refute @shell.exit_requested?
  end

  def test_errexit_still_exits_for_the_last_failed_and_command
    assert_equal 1, @shell.run('set -e; true && false; printf no')
    assert_equal "", @shell.stdout.string
    assert @shell.exit_requested?
  end

  def test_set_plus_e_continues
    assert_equal 0, @shell.run("set +e; false; printf yes")
    assert_equal "yes", @shell.stdout.string
  end

  def test_read_assigns_multiple_variables
    @shell.stdin = StringIO.new("hello world rest\n")
    assert_equal 0, @shell.run("read A B")
    assert_equal "hello", @shell.env["A"]
    assert_equal "world rest", @shell.env["B"]
  end

  def test_read_trims_leading_space_and_preserves_spacing_in_the_last_variable
    @shell.stdin = StringIO.new("  hello   world  rest  \n")

    assert_equal 0, @shell.run("read A B")
    assert_equal "hello", @shell.env["A"]
    assert_equal "world  rest", @shell.env["B"]
  end

  def test_read_treats_escaped_spaces_as_part_of_a_field
    @shell.stdin = StringIO.new("hello\\ world tail\n")

    assert_equal 0, @shell.run("read A B")
    assert_equal "hello world", @shell.env["A"]
    assert_equal "tail", @shell.env["B"]
  end

  def test_read_joins_backslash_newline_continuations
    @shell.stdin = StringIO.new("first\\\nsecond\n")

    assert_equal 0, @shell.run("read VALUE")
    assert_equal "firstsecond", @shell.env["VALUE"]
  end

  def test_read_eof_and_raw
    @shell.stdin = StringIO.new("")
    assert_equal 1, @shell.run("read -r A")
    @shell.stdin = StringIO.new("literal\\n\n")
    assert_equal 0, @shell.run("read -r A")
    assert_equal "literal\\n", @shell.env["A"]
  end

  def test_set_prints_options_and_rejects_unknown
    assert_equal 0, @shell.run("set -e")
    assert_equal 0, @shell.run("set")
    assert_includes @shell.stdout.string, "errexit"
    assert_equal 1, @shell.run("set -Z")
    assert_match(/invalid option/, @shell.stderr.string)
  end

  def test_command_substitution
    assert_equal 0, @shell.run("printf '%s' \"$(printf sub)\"")
    assert_equal "sub", @shell.stdout.string
  end

  def test_command_substitution_strips_trailing_newlines
    assert_equal 0, @shell.run("printf '<%s>' \"$(printf 'x\\n\\n')\"")
    assert_equal "<x>", @shell.stdout.string
  end

  def test_nounset_and_recovery
    assert_equal 1, @shell.run("set -u; printf '%s' \"$MISSING\"")
    assert_match(/MISSING: unbound variable/, @shell.stderr.string)
    assert_equal 0, @shell.run("set +u; printf '%s' \"$MISSING\"")
    assert_equal 0, @shell.run("set -u; printf '%s' \"${MISSING:-ok}\"")
    assert_includes @shell.stdout.string, "ok"
  end

  def test_xtrace_prints_expanded_command
    assert_equal 0, @shell.run("set -x; echo traced")
    assert_includes @shell.stderr.string, "+ echo traced"
    assert_includes @shell.stdout.string, "traced"
  end

  def test_pipeline_sets_status_before_the_next_command
    assert_equal 0, @shell.run('false | false; printf "%s" "$?"')
    assert_equal "1", @shell.stdout.string
  end

  def test_subshell_sets_status_before_the_next_command
    assert_equal 0, @shell.run('(false); printf "%s" "$?"', compound: true)
    assert_equal "1", @shell.stdout.string
  end

  def test_if_without_a_matching_branch_sets_zero_status
    assert_equal 0, @shell.run('if false; then true; fi; printf "%s" "$?"', compound: true)
    assert_equal "0", @shell.stdout.string
  end

  def test_negation_sets_status_before_the_next_command
    assert_equal 0, @shell.run('! true; printf "%s" "$?"', compound: true)
    assert_equal "1", @shell.stdout.string
  end

  def test_assignment_with_command_substitution_uses_its_status
    assert_equal 0, @shell.run('VALUE=$(false); printf "%s" "$?"')
    assert_equal "1", @shell.stdout.string
  end

  def test_double_brackets_expand_variables_without_field_splitting
    assert_equal 0, @shell.run('VALUE="two words"; [[ $VALUE == "two words" ]]', compound: true)
  end

  def test_double_brackets_match_unquoted_patterns
    assert_equal 0, @shell.run('[[ sharky == sh* ]]', compound: true)
  end

  def test_double_brackets_expand_variable_patterns
    assert_equal 0, @shell.run('VALUE=sharky; PATTERN="sh*"; [[ $VALUE == $PATTERN ]]', compound: true)
  end

  def test_double_brackets_treat_quoted_patterns_as_literals
    assert_equal 1, @shell.run('[[ sharky == "sh*" ]]', compound: true)
    assert_equal 0, @shell.run('[[ "sh*" == "sh*" ]]', compound: true)
  end

  def test_empty_double_brackets_report_a_syntax_error
    assert_equal 2, @shell.run('[[ ]]', compound: true)
    assert_match(/syntax error/, @shell.stderr.string)
  end

  def test_pipefail
    assert_equal 1, @shell.run("set -o pipefail; false | true")
    assert_equal 0, @shell.run("set +o pipefail; false | true")
  end
end
