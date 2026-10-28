module Sharky
  module AST
    Program = Struct.new(:lines, keyword_init: true)
    Line = Struct.new(:list, keyword_init: true)
    List = Struct.new(:entries, keyword_init: true)
    AndOr = Struct.new(:first, :rest, keyword_init: true) do
      def commands
        first.commands
      end
    end
    Pipeline = Struct.new(:commands, keyword_init: true)
    Simple = Struct.new(:assignments, :words, :redirects, keyword_init: true)
    Subshell = Struct.new(:list, keyword_init: true)
    Assignment = Struct.new(:name, :value, keyword_init: true)
    Redirect = Struct.new(:fd, :op, :target, keyword_init: true)
    Word = Struct.new(:parts, :pos, keyword_init: true)
    Part = Struct.new(:type, :value, :pos, keyword_init: true)
  end
end
