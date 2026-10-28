# frozen_string_literal: true

module Sharky
  module Builtins
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
