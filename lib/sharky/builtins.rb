# frozen_string_literal: true

require "shellwords"

module Sharky
  module Builtins
    SIGNALS = %w[
      HUP INT QUIT ILL TRAP ABRT BUS FPE KILL USR1 SEGV USR2 PIPE ALRM TERM
      CHLD CONT STOP TSTP TTIN TTOU URG XCPU XFSZ VTALRM PROF WINCH IO SYS
    ].freeze

    TABLE = {
      "cd" => lambda do |shell, argv, _stdin|
        shell.cd(argv.first)
      end,
      "z" => lambda do |shell, argv, _stdin|
        Builtins.z_builtin(shell, argv)
      end,
      "zi" => lambda do |shell, argv, _stdin|
        Builtins.zi_builtin(shell, argv)
      end,
      "pwd" => lambda do |shell, _argv, _stdin|
        shell.stdout.puts(shell.cwd)
        0
      end,
      "source" => lambda do |shell, argv, _stdin|
        Builtins.source_builtin(shell, argv)
      end,
      "." => lambda do |shell, argv, _stdin|
        Builtins.source_builtin(shell, argv)
      end,
      "export" => lambda do |shell, argv, _stdin|
        if argv.empty?
          shell.env.each { |name, value| shell.stdout.puts("#{name}=#{value}") }
        else
          argv.each do |arg|
            name, value = arg.split("=", 2)
            shell.export(name, value)
          end
        end
        0
      end,
      "unset" => lambda do |shell, argv, _stdin|
        argv.each { |name| shell.unset(name) }
        0
      end,
      "reload" => lambda do |shell, _argv, _stdin|
        shell.runtime.reload_config.each { |warning| shell.stderr.puts(warning) }
        0
      end,
      "jobs" => lambda do |shell, argv, _stdin|
        shell.jobs_builtin(argv)
      end,
      "fg" => lambda do |shell, argv, _stdin|
        shell.fg_builtin(argv)
      end,
      "bg" => lambda do |shell, argv, _stdin|
        shell.bg_builtin(argv)
      end,
      "wait" => lambda do |shell, argv, _stdin|
        shell.wait_builtin(argv)
      end,
      "kill" => lambda do |shell, argv, _stdin|
        shell.kill_builtin(argv)
      end,
      "history" => lambda do |shell, argv, _stdin|
        Builtins.history_builtin(shell, argv)
      end,
      "alias" => lambda do |shell, argv, _stdin|
        Builtins.alias_builtin(shell, argv)
      end,
      "unalias" => lambda do |shell, argv, _stdin|
        Builtins.unalias_builtin(shell, argv)
      end,
      "abbr" => lambda do |shell, argv, _stdin|
        Builtins.abbr_builtin(shell, argv)
      end,
      "unabbr" => lambda do |shell, argv, _stdin|
        Builtins.unabbr_builtin(shell, argv)
      end,
      "type" => lambda do |shell, argv, _stdin|
        Builtins.type_builtin(shell, argv)
      end,
      "exit" => lambda do |shell, argv, _stdin|
        status = argv.empty? ? shell.last_status : Integer(argv.first, 10)
        shell.request_exit(status)
      rescue ArgumentError
        shell.stderr.puts("sharky: exit: #{argv.first}: numeric argument required")
        shell.request_exit(2)
      end,
      "break" => lambda do |shell, argv, _stdin|
        Builtins.break_builtin(shell, argv)
      end,
      "continue" => lambda do |shell, argv, _stdin|
        Builtins.continue_builtin(shell, argv)
      end,
      "return" => lambda do |shell, argv, _stdin|
        Builtins.return_builtin(shell, argv)
      end,
      "shift" => lambda do |shell, argv, _stdin|
        Builtins.shift_builtin(shell, argv)
      end,
      "set" => lambda do |shell, argv, _stdin|
        Builtins.set_builtin(shell, argv)
      end,
      "read" => lambda do |shell, argv, _stdin|
        Builtins.read_builtin(shell, argv)
      end,
      "trap" => lambda do |shell, argv, _stdin|
        Builtins.trap_builtin(shell, argv)
      end
    }.freeze

    def self.lookup(name)
      TABLE[name]
    end

    def self.alias_builtin(shell, argv)
      aliases = shell.runtime.aliases
      if argv.empty?
        aliases.sort.each { |name, value| shell.stdout.puts("alias #{name}='#{value}'") }
        return 0
      end

      status = 0
      index = 0
      while index < argv.length
        argument = argv[index]
        if argument.include?("=")
          name, value = argument.split("=", 2)
          index += 1
        elsif index + 1 < argv.length
          name = argument
          value = argv[index + 1]
          index += 2
        else
          name = argument
          index += 1
          if aliases.key?(name)
            shell.stdout.puts("alias #{name}='#{aliases[name]}'")
          else
            shell.stderr.puts("sharky: alias: #{name}: not found")
            status = 1
          end
          next
        end

        if name.empty?
          shell.stderr.puts("sharky: alias: empty alias name")
          status = 1
        elsif Runtime.control_operator?(value)
          shell.stderr.puts("sharky: alias: #{name}: control operator in alias value")
          status = 1
        else
          aliases[name] = value
        end
      end
      status
    end

    def self.unalias_builtin(shell, argv)
      if argv.empty?
        shell.stderr.puts("sharky: unalias: usage: unalias [-a] name ...")
        return 1
      end

      aliases = shell.runtime.aliases
      return (aliases.clear && 0) if argv == ["-a"]

      status = 0
      argv.each do |name|
        unless aliases.delete(name)
          shell.stderr.puts("sharky: unalias: #{name}: not found")
          status = 1
        end
      end
      status
    end

    def self.abbr_builtin(shell, argv)
      abbreviations = shell.runtime.abbreviations
      if argv.empty?
        abbreviations.sort.each { |name, value| shell.stdout.puts("abbr #{name}='#{value}'") }
        return 0
      end

      status = 0
      index = 0
      while index < argv.length
        argument = argv[index]
        if argument.include?("=")
          name, value = argument.split("=", 2)
          index += 1
        elsif index + 1 < argv.length
          name = argument
          value = argv[index + 1]
          index += 2
        else
          name = argument
          index += 1
          if abbreviations.key?(name)
            shell.stdout.puts("abbr #{name}='#{abbreviations[name]}'")
          else
            shell.stderr.puts("sharky: abbr: #{name}: not found")
            status = 1
          end
          next
        end

        abbreviations[name] = value
      end
      status
    end

    def self.unabbr_builtin(shell, argv)
      if argv.empty?
        shell.stderr.puts("sharky: unabbr: usage: unabbr [-a] name ...")
        return 1
      end

      abbreviations = shell.runtime.abbreviations
      return (abbreviations.clear && 0) if argv == ["-a"]

      status = 0
      argv.each do |name|
        unless abbreviations.delete(name)
          shell.stderr.puts("sharky: unabbr: #{name}: not found")
          status = 1
        end
      end
      status
    end

    def self.history_builtin(shell, argv)
      history = shell.history
      unless history
        shell.stderr.puts("sharky: history: not available")
        return 1
      end

      if argv.empty?
        history.entries.each { |entry| shell.stdout.puts(entry) }
        0
      elsif argv == ["-c"]
        history.clear
        0
      else
        shell.stderr.puts("sharky: history: usage: history [-c]")
        1
      end
    end

    def self.z_builtin(shell, argv)
      return shell.cd(nil) if argv.empty?

      if argv == ["-"]
        oldpwd = shell.env["OLDPWD"]
        if oldpwd.nil? || oldpwd.empty?
          shell.stderr.puts("sharky: z: OLDPWD not set")
          return 1
        end

        return shell.cd(oldpwd)
      end

      store = shell.z
      target = store&.query(argv)
      return no_z_match(shell, "z", argv) unless target

      shell.cd(target)
    end

    def self.zi_builtin(shell, argv)
      store = shell.z
      candidates = store ? store.matches(argv) : []
      return no_z_match(shell, "zi", argv) if candidates.empty?

      if shell.terminal&.tty?
        items = candidates.map do |path|
          Completion::Candidate.new(value: path, description: "dir")
        end
        pager = Pager.new(terminal: shell.terminal, theme: theme_for(shell))
        pick = proc { pager.select(items, title: "z") }
        choice = shell.terminal.respond_to?(:with_raw) ? shell.terminal.with_raw(&pick) : pick.call
        return 0 unless choice

        shell.cd(choice.value)
      else
        shell.cd(candidates.first)
      end
    end

    def self.theme_for(shell)
      configured = shell.runtime&.theme
      configured = configured.styles if configured.is_a?(Theme)
      Theme.new(configured || {})
    end
    private_class_method :theme_for

    def self.no_z_match(shell, command, argv)
      query = argv.join(" ")
      if query.empty?
        shell.stderr.puts("sharky: #{command}: no directories visited")
      else
        shell.stderr.puts("sharky: #{command}: no match: #{query}")
      end
      1
    end
    private_class_method :no_z_match

    def self.type_builtin(shell, argv)
      if argv.empty?
        shell.stderr.puts("sharky: type: usage: type name ...")
        return 1
      end

      status = 0
      argv.each do |name|
        aliases = shell.runtime.aliases
        if aliases.key?(name)
          shell.stdout.puts("#{name}: aliased to #{aliases[name]}")
        elsif shell.runtime.commands.key?(name) || shell.runtime.commands.key?(name.to_sym)
          shell.stdout.puts("#{name}: function")
        elsif Builtins.lookup(name)
          shell.stdout.puts("#{name}: shell builtin")
        elsif (path = executable_path(shell, name))
          shell.stdout.puts("#{name}: #{path}")
        else
          shell.stderr.puts("sharky: type: #{name}: not found")
          status = 1
        end
      end
      status
    end

    def self.break_builtin(shell, argv)
      levels = control_level(shell, "break", argv)
      return 1 unless levels

      raise ControlFlow::Break.new(levels)
    end

    def self.continue_builtin(shell, argv)
      levels = control_level(shell, "continue", argv)
      return 1 unless levels

      raise ControlFlow::Continue.new(levels)
    end

    def self.return_builtin(shell, argv)
      status = argv.empty? ? shell.last_status : Integer(argv.first, 10)
      raise ControlFlow::Return.new(status)
    rescue ArgumentError
      shell.stderr.puts("sharky: return: #{argv.first}: numeric argument required")
      1
    end

    def self.control_level(shell, name, argv)
      return 1 if argv.empty?

      level = Integer(argv.first, 10)
      return level if level.positive?

      shell.stderr.puts("sharky: #{name}: #{argv.first}: invalid level")
      nil
    rescue ArgumentError
      shell.stderr.puts("sharky: #{name}: #{argv.first}: numeric argument required")
      nil
    end
    private_class_method :control_level

    def self.set_builtin(shell, argv)
      args = argv.dup
      if args.first == "--"
        args.shift
        shell.positional = args
        return 0
      end

      if args.empty?
        shell.stdout.puts(shell.active_options.join(" ")) unless shell.active_options.empty?
        return 0
      end

      status = 0
      until args.empty?
        argument = args.shift
        if %w[-e +e -u +u -x +x].include?(argument)
          name = { "e" => :errexit, "u" => :nounset, "x" => :xtrace }.fetch(argument[1])
          shell.set_option(name, argument.start_with?("-"))
        elsif %w[-o +o].include?(argument)
          name = args.shift
          if name.nil?
            shell.stderr.puts("sharky: set: #{argument}: option name required")
            status = 1
          elsif name == "pipefail"
            shell.set_option(:pipefail, argument == "-o")
          else
            shell.stderr.puts("sharky: set: #{name}: invalid option name")
            status = 1
          end
        else
          shell.stderr.puts("sharky: set: #{argument}: invalid option")
          status = 1
        end
      end
      status
    end

    def self.trap_builtin(shell, argv)
      if %w[-l -L].include?(argv.first)
        shell.stdout.puts((["EXIT"] + SIGNALS).uniq.join(" "))
        return 0
      end

      if argv.empty?
        shell.traps.each do |signal, command|
          shell.stdout.puts("trap -- '#{command}' #{signal}")
        end
        return 0
      end

      args = argv.dup
      if args.first == "--"
        args.shift
      elsif args.first == "-"
        args.shift
        return 0 if args.empty?

        signals = args.map { |name| normalize_signal(name) }
        if signals.any?(&:nil?)
          shell.stderr.puts("sharky: trap: invalid signal specification")
          return 1
        end
        signals.each { |signal| shell.clear_trap(signal) }
        return 0
      end

      action = args.shift
      if action.nil?
        shell.stderr.puts("sharky: trap: action required")
        return 1
      end

      signals = args.map { |name| normalize_signal(name) }
      if signals.empty? || signals.any?(&:nil?)
        shell.stderr.puts("sharky: trap: invalid signal specification")
        return 1
      end

      signals.each { |signal| shell.set_trap(signal, action) }
      0
    end

    def self.normalize_signal(name)
      signal = name.to_s.upcase
      signal = "EXIT" if signal == "0"
      signal = signal.delete_prefix("SIG")
      return signal if signal == "EXIT" || SIGNALS.include?(signal)

      nil
    end

    def self.read_builtin(shell, argv)
      args = argv.dup
      raw = args.first == "-r"
      args.shift if raw
      names = args.empty? ? ["REPLY"] : args
      tokens = []
      status = 1
      while (line = shell.stdin.gets)
        parts = raw ? line.chars : line.scan(/\\.|./m)
        if !raw && parts.last == "\\\n"
          tokens.concat(parts[0...-1])
          next
        end
        tokens.concat(parts)
        status = line.end_with?("\n") ? 0 : 1
        break
      end
      tokens.pop if tokens.last == "\n"
      separator = ->(token) { token && " \t\n".include?(token) }
      tokens.shift while separator.call(tokens.first)
      tokens.pop while separator.call(tokens.last)
      names.each_with_index do |name, index|
        count = index == names.length - 1 ? tokens.length : (tokens.index(&separator) || tokens.length)
        value = tokens.shift(count).join
        shell.env[name] = raw ? value : value.gsub(/\\(.)/m, '\1')
        tokens.shift while separator.call(tokens.first)
      end
      status
    end

    def self.source_builtin(shell, argv)
      if argv.empty?
        shell.stderr.puts("sharky: source: filename argument required")
        return 1
      end

      shell.source_file(argv.first, argv.drop(1))
    end

    def self.shift_builtin(shell, argv)
      count = argv.empty? ? 1 : Integer(argv.first, 10)
      shell.shift(count)
    rescue ArgumentError
      shell.stderr.puts("sharky: shift: #{argv.first}: numeric argument required")
      1
    end

    def self.executable_path(shell, name)
      if name.include?("/")
        path = File.expand_path(name, shell.cwd)
        return path if File.file?(path) && File.executable?(path)

        return nil
      end

      shell.env["PATH"].to_s.split(File::PATH_SEPARATOR).each do |directory|
        directory = shell.cwd if directory.empty?
        path = File.join(directory, name)
        return path if File.file?(path) && File.executable?(path)
      end
      nil
    end
    private_class_method :executable_path
  end
end
