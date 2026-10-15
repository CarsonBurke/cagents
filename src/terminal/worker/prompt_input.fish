# The exit status belongs to the user's prompt, including themed prompts.
function __cagents_restore_prompt_status
    return $argv[1]
end
functions --copy fish_prompt __cagents_original_prompt
function fish_prompt
    set -l previous_status $status
    printf '\e]133;A\a'
    __cagents_restore_prompt_status $previous_status
    __cagents_original_prompt
    set -l prompt_status $status
    printf '\e]133;B\a'
    return $prompt_status
end
if functions --query fish_right_prompt
    functions --copy fish_right_prompt __cagents_original_right_prompt
    function fish_right_prompt
        set -l previous_status $status
        printf '\e]133;A;k=s\a'
        __cagents_restore_prompt_status $previous_status
        __cagents_original_right_prompt
        set -l prompt_status $status
        printf '\e]133;B\a'
        return $prompt_status
    end
end
function __cagents_preexec --on-event fish_preexec
    printf '\e]133;C\a'
end
function __cagents_postexec --on-event fish_postexec
    printf '\e]133;D;%s\a' $status
end
