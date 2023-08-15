import { PromptHandler } from './PromptHandler.js';
import { Question } from './Question.js';
import { Conversation } from './Conversation.js';
import { Action } from './Action.js';
import StartupManager from '/assets/js/modules/StartupManager/StartupManager.js';
import { getDayOfWeek, formatDate, formatTime12Hour} from '/assets/js/utilities/DateTime.js';
import { initiateDownload } from '/assets/js/utilities/Download.js';
import { CalculatingEngine } from '/assets/js/modules/Calculator/CalculatingEngine.js';
import { GITHUB_PAGE, LINKEDIN_PAGE, RESUME_TXT_PATH, RESUME_PDF_PATH, RESUME_HTML_PATH, RESUME_FILE_NAME, CALCULATOR_PATH } from '/assets/js/utilities/Constants.js';


export class Actions {
    constructor(terminal) {


        this.help = new Action(() => {
            let commands = Object.keys(this).join(', ');
            // let basicTerminalOperations = ['help', 'clear', 'date', 'shutdown'];
            // let resumeActions = ['resume', 'download resume', 'open resume', 'open html resume'];
            // let socialMediaActions = ['github', 'linkedin'];
            
            // let helpOutput = '';
        
            // helpOutput += 'Basic Terminal Operations: ' + basicTerminalOperations.join(', ') + '<br></br>';
            // helpOutput += 'Resume Actions: ' + resumeActions.join(', ') + '<br></br>';
            // helpOutput += 'Social Media Actions: ' + socialMediaActions.join(', ') + '<br></br>';
            
            return commands;
        });

        // Add the 'calculate' action
        this.calculate = new Action(() => {
            // Set the free input processor function
            terminal.inputHandler.freeInputProcessor = (input) => {
                const calculateEngine = new CalculatingEngine();
                // Here, we can use eval or any other safe evaluator for math expressions
                try {
                    return calculateEngine.evaluate(input);
                    // return eval(input);
                } catch (error) {
                    return error;
                }
            };
            return "Enter your mathematical expression. Type 'exit' to leave this mode.";
        });
        

        this.clear = new Action(() => {
            terminal.outputHandler.clearOutput();
            return '';
        });

        this.resume = new Action(() => {
            const startupManager = new StartupManager(terminal.interactiveWindows, terminal.observable);
            const RESUME_QUESTION = `
                What would you like to do with Paul Moscuzza's resume?
                <br></br>
                1. Download the resume
                <br></br>
                2. View the resume
                <br></br>
                3. Open the resume as a PDF
                <br></br>
                4. View the resume as HTML
                <br></br>
                5. Exit this mode
                <br></br>
                Type the number corresponding to your choice.`;
    
            const question = new Question(RESUME_QUESTION, {
                '1': new Action(() => {
                    initiateDownload(RESUME_PDF_PATH, RESUME_FILE_NAME);
                    const promptHandler = new PromptHandler(terminal, `Initiating download of Paul Moscuzza's resume`);
                    promptHandler.handle();
                }),
                '2': this.createFetchAction(RESUME_TXT_PATH, (result) => {
                    terminal.outputHandler.setTypeSpeed(-5);
                    terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                        if (!isRunning) {
                            terminal.outputHandler.setTypeSpeed(0); // originalTypeSpeed
                            const promptHandler = new PromptHandler(terminal);
                            promptHandler.handle();
                        }
                    });
                    return result;
                }),
                '3': new Action(() => {
                    startupManager.startPDFViewer(RESUME_PDF_PATH, RESUME_HTML_PATH, `PDF Resume`);
                    const promptHandler = new PromptHandler(terminal, `Opening Paul Moscuzza's PDF resume`);
                    promptHandler.handle();
                }),
                '4': new Action(() => {
                    startupManager.startHTMLViewer(RESUME_HTML_PATH, `HTML Resume`);
                    const promptHandler = new PromptHandler(terminal, `Opening HTML version of Paul Moscuzza's resume`);
                    promptHandler.handle();
                }),
                '5': new Action(() => {
                    const promptHandler = new PromptHandler(terminal, 'Exiting resume mode. Let me know if you need anything else!');
                    promptHandler.handle();
                })
            });
    
            const conversation = new Conversation(question, terminal, terminal.lastCommand());
            conversation.start();
            return '';
        });
        
        this['open calculator'] = new Action(() => {
            const startupManager = new StartupManager(terminal.interactiveWindows, terminal.observable);
            startupManager.startHTMLViewer(CALCULATOR_PATH, `Calculator`);
            return `Opening Calculator`;
        });

        this.github = this.createSocialMediaAction(GITHUB_PAGE, terminal);
        this.linkedin = this.createSocialMediaAction(LINKEDIN_PAGE, terminal);

        this.date = new Action(() => {
            const currentDate = new Date();
            return `${getDayOfWeek(currentDate)} ${formatDate(currentDate)} ${formatTime12Hour(currentDate)} `;
        });

        this.exit = new Action(() => {
            terminal.terminalUI.destroyTerminal();
            return '';
        })

        this.shutdown = this.createFetchAction('/assets/templates/text/shutdown.txt', (result) => {
            terminal.terminalEvents.removeCtrlCListener();

            terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                if (!isRunning) {
                    setTimeout(() => {
                        terminal.observable.notify("windowShutdown", { source: terminal, message: 'shutdown' });
                        terminal.terminalUI.destroyTerminal();
                    }, 2000);
                }
            });

            return result;
        });
    }

    createFetchAction(url, onSuccess) {
        return new Action(async () => {
            try {
                const response = await fetch(url);
                if (!response.ok) {
                    throw new Error(`Failed to fetch from ${url}`);
                }
                const result = await response.text();
                return onSuccess(result);
            } catch (error) {
                return `Error: ${error.message}`;
            }
        });
    }

    createSocialMediaAction(pageUrl, terminal) {
        const startupManager = new StartupManager(terminal.interactiveWindows, terminal.observable);
        return new Action(() => {
            const QUESTION = `
                <a href="${pageUrl}" target="_blank">${pageUrl}</a>
                <br></br>
                Would you like me to redirect you to that page?
                <br></br>
                Type 'yes' or 'no'`;

            const questions = new Question(QUESTION, {
                yes: new Action(() => {
                    const promptHandler = new PromptHandler(terminal, `Opening ${pageUrl} in a new tab`);
                    promptHandler.handle();
                    
                    startupManager.startOpenPage(pageUrl);
                }),
                no: new Action(() => {
                    const promptHandler = new PromptHandler(terminal, 'Okay, let me know if you need anything else!');
                    promptHandler.handle();
                })
            });

            const conversation = new Conversation(questions, terminal, terminal.lastCommand());
            conversation.start();
            return '';
        });
    }
}
