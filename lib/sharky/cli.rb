# frozen_string_literal: true

require "json"
require "socket"
require "stringio"

module Sharky
  class CLI
    USAGE = <<~TEXT
      Usage: sharky [OPTIONS] [FILE.rb]
             sharky state --json [--no-config]

        -c COMMAND   run COMMAND and exit
        --no-config  do not load the configuration file
        -i           force interactive mode
        --json       print one JSON object for -c or state
        state        print shell state as JSON (requires --json)
        --version    print the version
        --help       print this help
    TEXT

    def self.start(argv, stdin: $stdin, stdout: $stdout, stderr: $stderr)
      command = nil
      script = nil
      script_args = []
      interactive = false
      load_config = true
      json = false
      state = false
      args = argv.dup

      until args.empty?
        arg = args.shift
        case arg
        when "-c"
          if args.empty?
            stderr.puts("sharky: -c: option requires an argument")
            return 2
          end
          command = args.shift
        when "--no-config"
          load_config = false
        when "--json"
          json = true
        when "-i"
          interactive = true
        when "--version"
          stdout.puts("sharky #{VERSION}")
          return 0
        when "--help", "-h"
          stdout.write(USAGE)
          return 0
        else
          if arg.start_with?("-")
            stderr.puts("sharky: unknown option: #{arg}")
            return 2
          end
          if arg == "state" && script.nil? && !state
            state = true
            next
          end
          script = arg
          script_args = args.dup
          args.clear
        end
      end

      if script && command
        stderr.puts("sharky: cannot use -c with a script")
        return 2
      end
      if state && command
        stderr.puts("sharky: cannot use state with -c")
        return 2
      end
      if state && script
        stderr.puts("sharky: cannot use state with a script")
        return 2
      end
      if state && !json
        stderr.puts("sharky: state requires --json")
        return 2
      end
      if json && script
        stderr.puts("sharky: --json is only supported with -c or state")
        return 2
      end
      return run_json_state(stdout: stdout, stderr: stderr, load_config: load_config) if state

      if script.nil? && command.nil?
        tty = stdin.respond_to?(:tty?) && stdin.tty?
        return run_interactive(stdin: stdin, stdout: stdout, stderr: stderr, load_config: load_config) if interactive || tty

        stderr.puts("sharky: no command given")
        return 1
      end
      return run_json_command(command, stdin: stdin, stdout: stdout, stderr: stderr, load_config: load_config) if json

      runtime = Runtime.new
      if load_config
        Config.new.load_into(runtime).each { |warning| stderr.puts(warning) }
      else
        Sharky.runtime = runtime
      end

      shell = Shell.new(
        env: ENV.to_h,
        cwd: Dir.pwd,
        stdin: stdin,
        stdout: stdout,
        stderr: stderr,
        runtime: runtime,
        z: Z.new
      )
      return run_script(shell, script, script_args, stderr) if script

      shell.run(command)
      shell.last_status
    end

    def self.run_json_command(command, stdin:, stdout:, stderr:, load_config:)
      session = Session.new(env: ENV.to_h, cwd: Dir.pwd, stdin: stdin)
      Sharky.session = session
      load_runtime_config(session.runtime, stderr) if load_config
      result = session.run(command)
      write_json(stdout, {
        schema_version: result.schema_version,
        status: result.status,
        stdout: result.stdout,
        stderr: result.stderr,
        duration_ms: result.duration_ms,
        cwd: result.cwd,
        env_delta: result.env_delta
      })
      result.status
    end

    def self.run_json_state(stdout:, stderr:, load_config:)
      session = Session.new(env: ENV.to_h, cwd: Dir.pwd)
      Sharky.session = session
      load_runtime_config(session.runtime, stderr) if load_config
      write_json(stdout, {
        schema_version: Result::SCHEMA_VERSION,
        cwd: session.cwd,
        env: session.env.dup,
        last_status: session.last_status,
        pid: Process.pid
      })
      0
    end

    def self.load_runtime_config(runtime, stderr)
      captured = StringIO.new
      previous_stdout = $stdout
      $stdout = captured
      warnings = Config.new.load_into(runtime)
      warnings.each { |warning| stderr.puts(warning) }
    ensure
      $stdout = previous_stdout
      stderr.write(captured.string) unless captured.string.empty?
    end

    def self.write_json(io, payload)
      io.puts(JSON.generate(payload))
    end

    def self.run_script(shell, path, args, stderr)
      runtime = shell.runtime
      runtime.shell = shell
      previous_argv = ARGV.dup
      ARGV.replace(args)
      library = File.expand_path("..", __dir__)
      $LOAD_PATH.unshift(library) unless $LOAD_PATH.include?(library)

      begin
        load(File.expand_path(path))
        shell.last_status
      rescue SystemExit => error
        error.status
      rescue StandardError, ScriptError => error
        stderr.puts("sharky: #{path}: #{error.class}: #{error.message}")
        stderr.puts(error.backtrace) if ENV["SHARKY_DEBUG"] == "1" && error.backtrace
        1
      ensure
        ARGV.replace(previous_argv)
        runtime.shell = nil
      end
    end

    def self.run_interactive(stdin:, stdout:, stderr:, load_config:)
      terminal = Terminal.new(input: stdin, output: stdout, env: ENV)
      unless terminal.tty?
        stderr.puts("sharky: interactive mode requires a terminal")
        return 1
      end

      runtime = Runtime.new
      if load_config
        config = Config.new
        config.ensure_layout! unless ENV["SHARKY_CONFIG"]
        config.load_into(runtime).each { |warning| stderr.puts(warning) }
      else
        Sharky.runtime = runtime
      end

      history = History.new
      history.load
      shell = Shell.new(
        env: ENV.to_h,
        cwd: Dir.pwd,
        stdin: stdin,
        stdout: stdout,
        stderr: stderr,
        terminal: terminal,
        history: history,
        runtime: runtime,
        z: Z.new
      )
      theme = theme_for(runtime)
      duration_ms = 0
      terminal.buffer_lines { shell.pending_signal = "TSTP" }
      shell.enable_job_control

      with_terminal_signals(terminal, shell) do
        begin
          loop do
            shell.reap_jobs
            shell.notify_jobs
            line = terminal.with_raw do
              theme = theme_for(runtime)
              prompt = build_prompt(shell, runtime, theme, duration_ms)
              editor = Editor.new(
                terminal: terminal,
                history: history,
                prompt: prompt,
                theme: theme,
                runtime: runtime,
                output: stdout,
                shell: shell
              )
              value = editor.readline
              unless value.nil? || value.strip.empty?
                history.add(value)
                history.save
              end
              value
            end
            break if line.nil?

            if line.strip.empty?
              shell.clear_pending_signal
              duration_ms = 0
              next
            end

            started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
            begin
              shell.run(line)
            rescue Interrupt
              shell.take_terminal
              stdout.write("^C\r\n")
              shell.mark_status(130)
            end
            shell.reap_jobs
            shell.notify_jobs
            duration_ms = ((Process.clock_gettime(Process::CLOCK_MONOTONIC) - started) * 1000).round
            break if shell.exit_requested?
          end

          shell.last_status
        ensure
          history.save
          terminal.restore
        end
      end
    end

    def self.with_terminal_signals(terminal, shell)
      previous = {}
      %w[TERM HUP].each do |signal|
        next unless Signal.list.key?(signal)

        previous[signal] = Signal.trap(signal) do
          terminal.restore
          Signal.trap(signal, "DEFAULT")
          Process.kill(signal, Process.pid)
        end
      end

      %w[TTOU TTIN TSTP QUIT].each do |signal|
        next unless Signal.list.key?(signal)

        previous[signal] = Signal.trap(signal, "IGNORE")
      end

      if Signal.list.key?("INT")
        previous["INT"] = Signal.trap("INT") do
          if shell.foreground_pgid
            shell.pending_signal = "INT"
          else
            raise Interrupt
          end
        end
      end

      if Signal.list.key?("CHLD")
        previous["CHLD"] = Signal.trap("CHLD") do
          shell.reap_requested = true
        end
      end

      if Signal.list.key?("WINCH")
        previous["WINCH"] = Signal.trap("WINCH") do
          shell.redraw_requested = true
        end
      end

      yield
    ensure
      previous.each do |signal, handler|
        Signal.trap(signal, handler)
      rescue ArgumentError
        nil
      end
    end

    def self.theme_for(runtime)
      runtime.theme.is_a?(Theme) ? runtime.theme : Theme.new(runtime.theme || {})
    end

    def self.build_prompt(shell, runtime, theme, duration_ms)
      git = Prompt::Git.new(shell.cwd).info
      context = Prompt::Context.new(
        cwd: shell.cwd,
        home: shell.env["HOME"],
        status: shell.last_status,
        duration_ms: duration_ms,
        git_branch: git[:branch],
        git_dirty: git[:dirty],
        git_ahead: git[:ahead],
        git_behind: git[:behind],
        git_staged: git[:staged],
        git_modified: git[:modified],
        git_untracked: git[:untracked],
        os: Prompt.os_name,
        arch: Prompt.arch,
        ssh: ssh?,
        user: user,
        host: host,
        jobs_count: shell.jobs.all.count { |job| job.state == :running }
      )
      prompt = runtime.prompt
      if prompt.nil? && runtime.starship
        begin
          return Starship.prompt(context, **runtime.starship)
        rescue Starship::Error
          # Fall through to the theme layout or built-in prompt.
        end
      end
      if prompt.nil? && runtime.theme_layout
        layout = runtime.theme_layout
        prompt = ->(ctx) { layout.render(ctx, theme: theme) }
      end
      Prompt.render(context, theme: theme, prompt: prompt)
    end

    def self.ssh?
      %w[SSH_CONNECTION SSH_CLIENT SSH_TTY].any? { |name| ENV[name] && !ENV[name].empty? }
    end

    def self.user
      ENV["USER"] || ENV["LOGNAME"] || "user"
    end

    def self.host
      Socket.gethostname.split(".").first
    rescue StandardError
      "localhost"
    end

    private_class_method :run_json_command, :run_json_state, :load_runtime_config, :write_json,
                         :run_interactive, :with_terminal_signals, :theme_for, :build_prompt, :ssh?, :user, :host

    def initialize(argv, stdin: $stdin, stdout: $stdout, stderr: $stderr)
      @argv = argv.dup
      @stdin = stdin
      @stdout = stdout
      @stderr = stderr
    end

    def run
      self.class.start(@argv, stdin: @stdin, stdout: @stdout, stderr: @stderr)
    end
  end
end
